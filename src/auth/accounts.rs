use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::config::Config;
use crate::error::Result;

use super::token::refresh_access_token;

/// Account selection strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SelectionStrategy {
    /// Stay on current account until rate-limited > 2 minutes
    Sticky,
    /// Rotate to next account each request
    RoundRobin,
    /// Smart selection based on health, quota, and freshness
    #[default]
    Hybrid,
}

/// Per-model rate limit state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRateLimit {
    /// Unix timestamp when rate limit expires
    pub until: u64,
}

/// Per-model quota state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelQuota {
    /// Fraction of quota remaining (0.0 - 1.0)
    pub remaining_fraction: f64,
    /// Unix timestamp when quota resets
    pub reset_time: u64,
}

/// A single account with all its state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// Unique identifier
    pub id: String,
    /// Email address
    pub email: String,
    /// OAuth refresh token
    pub refresh_token: String,
    /// Project ID for Cloud Code API
    #[serde(default)]
    pub project_id: Option<String>,
    /// Whether this account is enabled
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Subscription tier (free, pro, ultra)
    #[serde(default)]
    pub subscription_tier: Option<String>,
    /// Per-model quota tracking
    #[serde(default)]
    pub quota: HashMap<String, ModelQuota>,
    /// Per-model rate limits
    #[serde(default)]
    pub rate_limits: HashMap<String, ModelRateLimit>,
    /// Health score (0.0 - 1.0, higher is better)
    #[serde(default = "default_health")]
    pub health_score: f64,
    /// Last time this account was used (unix timestamp)
    #[serde(default)]
    pub last_used: u64,
    /// Token bucket for rate limiting (0 - 50)
    #[serde(default = "default_tokens")]
    pub tokens_available: u32,
    /// Whether account has auth issues
    #[serde(default)]
    pub is_invalid: bool,
    /// Reason for invalid state
    #[serde(default)]
    pub invalid_reason: Option<String>,
    /// Per-account quota threshold override (0.0-1.0, None means use global)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_threshold: Option<f64>,
    /// Per-model quota threshold overrides (takes priority over account-level)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_quota_thresholds: HashMap<String, f64>,

    // Runtime state (not persisted)
    #[serde(skip)]
    pub access_token: Option<String>,
    #[serde(skip)]
    pub access_token_expires: Option<u64>,
}

fn default_true() -> bool {
    true
}

fn default_health() -> f64 {
    1.0
}

fn default_tokens() -> u32 {
    50
}

impl Account {
    /// Create a new account from OAuth credentials
    pub fn new(email: String, refresh_token: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            email,
            refresh_token,
            project_id: None,
            enabled: true,
            subscription_tier: None,
            quota: HashMap::new(),
            rate_limits: HashMap::new(),
            health_score: 1.0,
            last_used: 0,
            tokens_available: 50,
            is_invalid: false,
            invalid_reason: None,
            quota_threshold: None,
            model_quota_thresholds: HashMap::new(),
            access_token: None,
            access_token_expires: None,
        }
    }

    /// Check if access token is valid
    pub fn is_access_token_valid(&self) -> bool {
        match (self.access_token.as_ref(), self.access_token_expires) {
            (Some(_), Some(expires)) => {
                let now = now_secs();
                now + 60 < expires
            }
            _ => false,
        }
    }

    /// Check if account is rate-limited for a specific model
    pub fn is_rate_limited(&self, model: &str) -> bool {
        if let Some(limit) = self.rate_limits.get(model) {
            now_secs() < limit.until
        } else {
            false
        }
    }

    /// Get remaining rate limit time in seconds
    pub fn rate_limit_remaining(&self, model: &str) -> u64 {
        if let Some(limit) = self.rate_limits.get(model) {
            let now = now_secs();
            if now < limit.until {
                return limit.until - now;
            }
        }
        0
    }

    /// Set rate limit for a model
    pub fn set_rate_limit(&mut self, model: &str, until: u64) {
        self.rate_limits
            .insert(model.to_string(), ModelRateLimit { until });
    }

    /// Clear rate limit for a model
    pub fn clear_rate_limit(&mut self, model: &str) {
        self.rate_limits.remove(model);
    }

    /// Get quota fraction for a model (defaults to 1.0 if unknown)
    pub fn get_quota_fraction(&self, model: &str) -> f64 {
        self.quota
            .get(model)
            .map(|q| q.remaining_fraction)
            .unwrap_or(1.0)
    }

    /// Get effective quota threshold for a model
    ///
    /// Priority: per-model threshold > per-account threshold > global threshold
    pub fn get_effective_quota_threshold(&self, model: &str, global_threshold: f64) -> f64 {
        // 1. Check per-model threshold first (highest priority)
        if let Some(&threshold) = self.model_quota_thresholds.get(model) {
            return threshold;
        }

        // 2. Check per-account threshold
        if let Some(threshold) = self.quota_threshold {
            return threshold;
        }

        // 3. Fall back to global threshold
        global_threshold
    }

    /// Check if account quota is below the effective threshold for a model
    pub fn is_quota_below_threshold(&self, model: &str, global_threshold: f64) -> bool {
        let threshold = self.get_effective_quota_threshold(model, global_threshold);
        let quota = self.get_quota_fraction(model);
        quota < threshold
    }

    /// Check if account is usable (enabled, valid, not rate-limited)
    pub fn is_usable(&self, model: &str) -> bool {
        self.enabled && !self.is_invalid && !self.is_rate_limited(model)
    }

    /// Record successful request
    pub fn record_success(&mut self) {
        self.health_score = (self.health_score + 0.1).min(1.0);
        self.last_used = now_secs();
        self.is_invalid = false;
        self.invalid_reason = None;
    }

    /// Record failed request
    pub fn record_failure(&mut self) {
        self.health_score = (self.health_score - 0.2).max(0.0);
        self.last_used = now_secs();
    }

    /// Consume a token (returns false if no tokens available)
    pub fn consume_token(&mut self) -> bool {
        if self.tokens_available > 0 {
            self.tokens_available -= 1;
            true
        } else {
            false
        }
    }

    /// Refill tokens (called periodically)
    pub fn refill_tokens(&mut self, amount: u32) {
        self.tokens_available = (self.tokens_available + amount).min(50);
    }

    /// Get access token, refreshing if needed
    pub async fn get_access_token(&mut self, http_client: &super::HttpClient) -> Result<String> {
        if self.is_access_token_valid() {
            return Ok(self.access_token.clone().unwrap());
        }

        let (access_token, expires_in) =
            refresh_access_token(http_client, &self.refresh_token).await?;

        let now = now_secs();
        self.access_token = Some(access_token.clone());
        self.access_token_expires = Some(now + expires_in);

        Ok(access_token)
    }

    /// Load a single account (for backward compatibility)
    /// Returns the first enabled, valid account from the store
    pub fn load() -> Result<Option<Account>> {
        let store = AccountStore::load()?;
        Ok(store
            .accounts
            .into_iter()
            .find(|a| a.enabled && !a.is_invalid))
    }

    /// Save a single account (for backward compatibility)
    /// Creates or updates account in the store
    pub fn save(&self) -> Result<()> {
        let mut store = AccountStore::load().unwrap_or_default();
        store.add_account(self.clone());
        store.save()
    }
}

/// Store for multiple accounts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountStore {
    /// All accounts
    pub accounts: Vec<Account>,
    /// Currently active account ID (for sticky strategy)
    #[serde(default)]
    pub active_account_id: Option<String>,
    /// Selection strategy
    #[serde(default)]
    pub strategy: SelectionStrategy,
    /// Global quota threshold (accounts below this are deprioritized)
    #[serde(default = "default_quota_threshold")]
    pub quota_threshold: f64,
}

fn default_quota_threshold() -> f64 {
    0.1
}

impl Default for AccountStore {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            active_account_id: None,
            strategy: SelectionStrategy::Hybrid,
            quota_threshold: 0.1,
        }
    }
}

impl AccountStore {
    /// Path to accounts file
    pub fn path() -> PathBuf {
        Config::dir().join("accounts.json")
    }

    /// Load accounts from disk
    pub fn load() -> Result<Self> {
        let path = Self::path();
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let store: AccountStore = serde_json::from_str(&content)?;
            tracing::info!(
                count = store.accounts.len(),
                "Loaded accounts from {}",
                path.display()
            );
            return Ok(store);
        }

        // Try to migrate from old single-account format
        if let Some(account) = Self::migrate_from_single_account()? {
            let mut store = AccountStore::default();
            store.accounts.push(account);
            store.save()?;
            return Ok(store);
        }

        // Try to import from JS proxy
        if let Some(store) = Self::import_from_js_proxy()? {
            store.save()?;
            return Ok(store);
        }

        Ok(AccountStore::default())
    }

    /// Save accounts to disk
    pub fn save(&self) -> Result<()> {
        let dir = Config::dir();
        std::fs::create_dir_all(&dir)?;
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::path(), content)?;
        Ok(())
    }

    /// Migrate from old single-account format
    fn migrate_from_single_account() -> Result<Option<Account>> {
        let old_path = Config::dir().join("account.json");
        if !old_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&old_path)?;

        #[derive(Deserialize)]
        struct OldAccount {
            email: String,
            refresh_token: String,
            project_id: Option<String>,
        }

        if let Ok(old) = serde_json::from_str::<OldAccount>(&content) {
            tracing::info!(email = %old.email, "Migrating from single-account format");
            let mut account = Account::new(old.email, old.refresh_token);
            account.project_id = old.project_id;

            // Rename old file as backup
            let backup_path = Config::dir().join("account.json.bak");
            let _ = std::fs::rename(&old_path, backup_path);

            return Ok(Some(account));
        }

        Ok(None)
    }

    /// Import accounts from JS proxy config
    fn import_from_js_proxy() -> Result<Option<AccountStore>> {
        let js_path = crate::config::dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("antigravity-proxy")
            .join("accounts.json");

        if !js_path.exists() {
            return Ok(None);
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct JsAccountFile {
            accounts: Vec<JsAccount>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct JsAccount {
            email: String,
            refresh_token: String,
            #[serde(default)]
            project_id: Option<String>,
            #[serde(default)]
            enabled: Option<bool>,
            #[serde(default)]
            is_invalid: Option<bool>,
        }

        let content = std::fs::read_to_string(&js_path)?;
        let js_file: JsAccountFile = serde_json::from_str(&content)?;

        if js_file.accounts.is_empty() {
            return Ok(None);
        }

        tracing::info!(
            count = js_file.accounts.len(),
            "Importing accounts from JS proxy"
        );

        let mut store = AccountStore::default();
        for js_account in js_file.accounts {
            let mut account = Account::new(js_account.email, js_account.refresh_token);
            account.project_id = js_account.project_id;
            account.enabled = js_account.enabled.unwrap_or(true);
            account.is_invalid = js_account.is_invalid.unwrap_or(false);
            store.accounts.push(account);
        }

        Ok(Some(store))
    }

    /// Add a new account
    pub fn add_account(&mut self, account: Account) {
        // Check if account with same email already exists
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.email == account.email) {
            // Update existing account
            existing.refresh_token = account.refresh_token;
            existing.enabled = true;
            existing.is_invalid = false;
            existing.invalid_reason = None;
            tracing::info!(email = %existing.email, "Updated existing account");
        } else {
            tracing::info!(email = %account.email, "Added new account");
            self.accounts.push(account);
        }
    }

    /// Remove an account by ID
    pub fn remove_account(&mut self, id: &str) -> bool {
        let len_before = self.accounts.len();
        self.accounts.retain(|a| a.id != id);

        // Clear active account if it was removed
        if self.active_account_id.as_deref() == Some(id) {
            self.active_account_id = None;
        }

        self.accounts.len() < len_before
    }

    /// Get mutable account by ID
    pub fn get_account_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    /// Set the active account
    pub fn set_active_account(&mut self, id: &str) {
        if self.accounts.iter().any(|a| a.id == id) {
            self.active_account_id = Some(id.to_string());
        }
    }

    /// Select best account for a request using configured strategy
    pub fn select_account(&mut self, model: &str) -> Option<String> {
        match self.strategy {
            SelectionStrategy::Sticky => self.select_sticky(model),
            SelectionStrategy::RoundRobin => self.select_round_robin(model),
            SelectionStrategy::Hybrid => self.select_hybrid(model),
        }
    }

    /// Sticky strategy: stay on current account until rate-limited
    fn select_sticky(&mut self, model: &str) -> Option<String> {
        // Check if active account is usable
        if let Some(id) = &self.active_account_id
            && let Some(account) = self.accounts.iter().find(|a| &a.id == id)
        {
            if account.is_usable(model) {
                return Some(id.clone());
            }
            // Check if rate limit is short (< 2 minutes) - wait instead of switch
            if account.rate_limit_remaining(model) < 120 {
                return Some(id.clone());
            }
        }

        // Find first usable account
        for account in &self.accounts {
            if account.is_usable(model) {
                self.active_account_id = Some(account.id.clone());
                return Some(account.id.clone());
            }
        }

        // Emergency: return any enabled account
        self.accounts
            .iter()
            .find(|a| a.enabled)
            .map(|a| a.id.clone())
    }

    /// Round-robin strategy: rotate to next account
    fn select_round_robin(&mut self, model: &str) -> Option<String> {
        let usable: Vec<_> = self
            .accounts
            .iter()
            .filter(|a| a.is_usable(model))
            .collect();

        if usable.is_empty() {
            return self
                .accounts
                .iter()
                .find(|a| a.enabled)
                .map(|a| a.id.clone());
        }

        // Find current index and rotate
        let current_idx = self
            .active_account_id
            .as_ref()
            .and_then(|id| usable.iter().position(|a| &a.id == id))
            .unwrap_or(0);

        let next_idx = (current_idx + 1) % usable.len();
        let selected = usable[next_idx].id.clone();
        self.active_account_id = Some(selected.clone());
        Some(selected)
    }

    /// Hybrid strategy: score-based selection
    fn select_hybrid(&mut self, model: &str) -> Option<String> {
        let now = now_secs();
        let global_threshold = self.quota_threshold;

        let mut candidates: Vec<_> = self
            .accounts
            .iter()
            .filter(|a| a.enabled && !a.is_invalid && !a.is_rate_limited(model))
            .filter(|a| !a.is_quota_below_threshold(model, global_threshold))
            .map(|a| {
                // Score formula: health*2 + tokens*5 + quota*3 + freshness*0.1
                let health_score = a.health_score * 2.0;
                let token_score = (a.tokens_available as f64 / 50.0) * 100.0 * 5.0;
                let quota_score = a.get_quota_fraction(model) * 100.0 * 3.0;
                let freshness = if a.last_used == 0 {
                    100.0
                } else {
                    ((now - a.last_used) as f64 / 60.0).min(100.0)
                };
                let freshness_score = freshness * 0.1;

                let total_score = health_score + token_score + quota_score + freshness_score;
                (a.id.clone(), total_score)
            })
            .collect();

        // Sort by score descending
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((id, _)) = candidates.first() {
            self.active_account_id = Some(id.clone());
            return Some(id.clone());
        }

        // Emergency fallback: any enabled account
        self.accounts.iter().find(|a| a.enabled).map(|a| {
            self.active_account_id = Some(a.id.clone());
            a.id.clone()
        })
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_account_new() {
        let account = Account::new("test@example.com".to_string(), "refresh_token".to_string());
        assert_eq!(account.email, "test@example.com");
        assert!(account.enabled);
        assert!(!account.is_invalid);
        assert_eq!(account.health_score, 1.0);
        assert_eq!(account.tokens_available, 50);
    }

    #[test]
    fn test_account_rate_limit() {
        let mut account = Account::new("test@example.com".to_string(), "token".to_string());

        // Not rate limited initially
        assert!(!account.is_rate_limited("model-a"));

        // Set rate limit
        let future = now_secs() + 60;
        account.set_rate_limit("model-a", future);
        assert!(account.is_rate_limited("model-a"));
        assert!(!account.is_rate_limited("model-b"));

        // Clear rate limit
        account.clear_rate_limit("model-a");
        assert!(!account.is_rate_limited("model-a"));
    }

    #[test]
    fn test_account_health() {
        let mut account = Account::new("test@example.com".to_string(), "token".to_string());

        account.record_failure();
        assert!(account.health_score < 1.0);

        account.record_success();
        assert!(account.health_score > 0.8);
    }

    #[test]
    fn test_account_store_add_remove() {
        let mut store = AccountStore::default();

        let account = Account::new("test@example.com".to_string(), "token".to_string());
        let id = account.id.clone();

        store.add_account(account);
        assert_eq!(store.accounts.len(), 1);

        assert!(store.remove_account(&id));
        assert_eq!(store.accounts.len(), 0);
    }

    #[test]
    fn test_hybrid_selection() {
        let mut store = AccountStore::default();

        let mut a1 = Account::new("a1@example.com".to_string(), "token1".to_string());
        a1.health_score = 0.5;

        let mut a2 = Account::new("a2@example.com".to_string(), "token2".to_string());
        a2.health_score = 1.0;

        store.add_account(a1);
        store.add_account(a2);

        // Should select a2 (higher health score)
        let selected = store.select_account("model").unwrap();
        assert_eq!(
            store
                .accounts
                .iter()
                .find(|a| a.id == selected)
                .unwrap()
                .email,
            "a2@example.com"
        );
    }

    #[test]
    fn test_per_account_quota_threshold() {
        let mut account = Account::new("test@example.com".to_string(), "token".to_string());

        // No overrides - should use global
        assert_eq!(account.get_effective_quota_threshold("model", 0.1), 0.1);

        // Per-account override
        account.quota_threshold = Some(0.2);
        assert_eq!(account.get_effective_quota_threshold("model", 0.1), 0.2);

        // Per-model override takes priority
        account
            .model_quota_thresholds
            .insert("model".to_string(), 0.3);
        assert_eq!(account.get_effective_quota_threshold("model", 0.1), 0.3);

        // Other models still use account threshold
        assert_eq!(account.get_effective_quota_threshold("other", 0.1), 0.2);
    }

    #[test]
    fn test_is_quota_below_threshold() {
        let mut account = Account::new("test@example.com".to_string(), "token".to_string());

        // Set quota to 0.15
        account.quota.insert(
            "model".to_string(),
            ModelQuota {
                remaining_fraction: 0.15,
                reset_time: 0,
            },
        );

        // With global threshold 0.1, not below
        assert!(!account.is_quota_below_threshold("model", 0.1));

        // With global threshold 0.2, below
        assert!(account.is_quota_below_threshold("model", 0.2));

        // Per-model threshold overrides
        account
            .model_quota_thresholds
            .insert("model".to_string(), 0.1);
        assert!(!account.is_quota_below_threshold("model", 0.2));
    }
}
