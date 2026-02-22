//! Request identity camouflage for upstream API calls.
//!
//! Constructs headers that make AGCP requests look indistinguishable from
//! a real Antigravity Electron desktop client.  All values are computed once
//! at startup and reused for every request.

use std::borrow::Cow;
use std::sync::LazyLock;

// ── Known-stable fingerprint library ────────────────────────────────────
// Antigravity 1.16.5 ships Electron 39.2.3 → Chromium 132.0.6834.160.
// Used as the fallback when no local installation is detected
// (Docker, headless servers, etc.).

pub const KNOWN_STABLE_VERSION: &str = "1.16.5";
const KNOWN_STABLE_ELECTRON: &str = "39.2.3";
const KNOWN_STABLE_CHROME: &str = "132.0.6834.160";

// ── Resolved version (computed once) ────────────────────────────────────

struct VersionConfig {
    version: String,
    electron: String,
    chrome: String,
}

/// Try to detect the locally installed Antigravity version.
///
/// Strategy: file-based detection only (no subprocess invocation).
/// This intentionally avoids launching `antigravity --version`, which can
/// spawn Electron/Chromium helper processes and block startup.
fn detect_local_version() -> Option<String> {
    // Linux: check common install paths for package.json (no subprocess needed)
    #[cfg(target_os = "linux")]
    {
        for base in &[
            "/usr/lib/antigravity",
            "/usr/share/antigravity",
            "/opt/Antigravity",
        ] {
            let pkg = std::path::Path::new(base).join("resources/app/package.json");
            if let Ok(content) = std::fs::read_to_string(&pkg)
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(v) = json.get("version").and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
        }
    }

    // macOS: read Info.plist from the app bundle
    #[cfg(target_os = "macos")]
    {
        let plist = std::path::Path::new("/Applications/Antigravity.app/Contents/Info.plist");
        if let Ok(content) = std::fs::read_to_string(plist)
            && let Some(pos) = content.find("CFBundleShortVersionString")
        {
            let after = &content[pos..];
            if let Some(s) = after.find("<string>") {
                let rest = &after[s + 8..];
                if let Some(e) = rest.find("</string>") {
                    let ver = &rest[..e];
                    if extract_semver(ver).is_some() {
                        return Some(ver.to_string());
                    }
                }
            }
        }
    }

    // Windows: check %LOCALAPPDATA%\Programs\Antigravity\resources\app\package.json
    #[cfg(target_os = "windows")]
    {
        if let Some(local_data) = dirs::data_local_dir() {
            let pkg = local_data
                .join("Programs")
                .join("Antigravity")
                .join("resources")
                .join("app")
                .join("package.json");
            if let Ok(content) = std::fs::read_to_string(&pkg)
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(v) = json.get("version").and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
        }
    }

    None
}

/// Extract the first `X.Y.Z` semver triple from an arbitrary string.
#[cfg(any(test, target_os = "macos"))]
fn extract_semver(raw: &str) -> Option<String> {
    for token in raw.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let t = token.trim_matches(|c: char| c == '"' || c == '\'' || c == '(' || c == ')');
        if t.is_empty() {
            continue;
        }
        // Strip optional leading 'v' or 'V' prefix (e.g. "v1.2.3")
        let t = t
            .strip_prefix('v')
            .or_else(|| t.strip_prefix('V'))
            .unwrap_or(t);
        let mut parts = t.split('.');
        let p1 = parts.next();
        let p2 = parts.next();
        let p3 = parts.next();
        if let (Some(a), Some(b), Some(c)) = (p1, p2, p3)
            && [a, b, c]
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|ch| ch.is_ascii_digit()))
        {
            return Some(format!("{}.{}.{}", a, b, c));
        }
    }
    None
}

/// Resolve the version config: local install → known stable fallback.
fn resolve_version_config() -> VersionConfig {
    if let Some(ver) = detect_local_version() {
        tracing::info!(
            version = %ver,
            source = "local_installation",
            "Fingerprint version detected"
        );
        return VersionConfig {
            version: ver,
            electron: KNOWN_STABLE_ELECTRON.to_string(),
            chrome: KNOWN_STABLE_CHROME.to_string(),
        };
    }

    tracing::info!(
        version = KNOWN_STABLE_VERSION,
        source = "known_stable_fallback",
        "No local Antigravity found; using known stable fingerprint"
    );
    VersionConfig {
        version: KNOWN_STABLE_VERSION.to_string(),
        electron: KNOWN_STABLE_ELECTRON.to_string(),
        chrome: KNOWN_STABLE_CHROME.to_string(),
    }
}

// ── Public statics ──────────────────────────────────────────────────────

/// Resolved Antigravity version (e.g. `"1.16.5"`).
pub static VERSION: LazyLock<String> = LazyLock::new(|| {
    let cfg = resolve_version_config();
    // Also trigger USER_AGENT evaluation so logging happens together
    let _ = &*USER_AGENT;
    cfg.version
});

/// Electron-style User-Agent that matches the official desktop client.
///
/// ```text
/// Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
///   Antigravity/1.16.5 Chrome/132.0.6834.160 Electron/39.2.3 Safari/537.36
/// ```
pub static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let cfg = resolve_version_config();
    let platform = match std::env::consts::OS {
        "macos" => "Macintosh; Intel Mac OS X 10_15_7",
        "windows" => "Windows NT 10.0; Win64; x64",
        _ => "X11; Linux x86_64",
    };
    format!(
        "Mozilla/5.0 ({}) AppleWebKit/537.36 (KHTML, like Gecko) Antigravity/{} Chrome/{} Electron/{} Safari/537.36",
        platform, cfg.version, cfg.chrome, cfg.electron
    )
});

/// Persistent machine identifier matching the Antigravity Electron client format.
///
/// Resolution order:
/// 1. `~/.config/Antigravity/machineid` — the real app's generated UUID (if installed)
/// 2. `~/.config/agcp/machineid` — our own persistent UUID (generated once, reused)
/// 3. Generate a new UUID v4, persist it at `~/.config/agcp/machineid`
pub static MACHINE_ID: LazyLock<Option<String>> = LazyLock::new(resolve_machine_id);

/// Resolve machine ID following the Antigravity client's approach.
fn resolve_machine_id() -> Option<String> {
    // 1. Try reading the real Antigravity app's machineid (UUID format)
    if let Some(config_dir) = dirs::config_dir() {
        let antigravity_mid = config_dir.join("Antigravity").join("machineid");
        if let Ok(id) = std::fs::read_to_string(&antigravity_mid) {
            let id = id.trim().to_string();
            if !id.is_empty() {
                tracing::debug!(source = "antigravity_app", "Machine ID loaded");
                return Some(id);
            }
        }
    }

    // 2. Try reading our own persisted machine ID
    let agcp_mid_path = crate::config::Config::dir().join("machineid");
    if let Ok(id) = std::fs::read_to_string(&agcp_mid_path) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            tracing::debug!(source = "agcp_persisted", "Machine ID loaded");
            return Some(id);
        }
    }

    // 3. Generate a new UUID v4 and persist it
    let new_id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = agcp_mid_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&agcp_mid_path, &new_id) {
        tracing::warn!(error = %e, "Failed to persist machine ID");
    } else {
        tracing::debug!(source = "generated", "Machine ID created and persisted");
    }
    Some(new_id)
}

/// Per-launch session identifier (UUID v4).
pub static SESSION_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());

// ── Header builder ──────────────────────────────────────────────────────

/// Build the full set of fingerprint headers for upstream API requests.
///
/// Returns headers that camouflage AGCP as the official Antigravity
/// Electron client across User-Agent, client identity, device identity,
/// and runtime environment layers.
pub fn build_fingerprint_headers() -> Vec<(Cow<'static, str>, Cow<'static, str>)> {
    let mut headers = Vec::with_capacity(8);

    // 1. User-Agent (Electron-style)
    headers.push((Cow::Borrowed("User-Agent"), Cow::Owned(USER_AGENT.clone())));

    // 2. Client identity
    headers.push((Cow::Borrowed("X-Client-Name"), Cow::Borrowed("antigravity")));
    headers.push((
        Cow::Borrowed("X-Client-Version"),
        Cow::Owned(VERSION.clone()),
    ));

    // 3. Device identity — persistent machine ID
    if let Some(ref mid) = *MACHINE_ID {
        headers.push((Cow::Borrowed("X-Machine-Id"), Cow::Owned(mid.clone())));
    }

    // 4. Session identity — per-launch UUID
    headers.push((
        Cow::Borrowed("X-VSCode-SessionId"),
        Cow::Owned(SESSION_ID.clone()),
    ));

    // 5. Node.js / Electron environment simulation
    headers.push((
        Cow::Borrowed("X-Goog-Api-Client"),
        Cow::Borrowed("gl-node/18.18.2 fire/0.8.6 grpc/1.10.x"),
    ));

    // 6. Per-request trace ID (unique per upstream call)
    headers.push((
        Cow::Borrowed("X-Request-Id"),
        Cow::Owned(uuid::Uuid::new_v4().to_string()),
    ));

    headers
}

// ── Diagnostic helpers (for `agcp doctor`) ──────────────────────────────

/// Returns a human-readable description of how the version was resolved.
pub fn version_source() -> &'static str {
    // Compare already-resolved version against fallback to determine source
    // (avoids re-running detect_local_version which would launch the GUI)
    if *VERSION == KNOWN_STABLE_VERSION {
        "known stable fallback"
    } else {
        "local installation"
    }
}

/// Returns a human-readable description of how the machine ID was resolved.
pub fn machine_id_source() -> &'static str {
    if let Some(config_dir) = dirs::config_dir() {
        let antigravity_mid = config_dir.join("Antigravity").join("machineid");
        if antigravity_mid.exists() {
            return "Antigravity app (~/.config/Antigravity/machineid)";
        }
    }
    let agcp_mid = crate::config::Config::dir().join("machineid");
    if agcp_mid.exists() {
        "AGCP persisted (~/.config/agcp/machineid)"
    } else {
        "generated (new UUID)"
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_agent_format() {
        let ua = USER_AGENT.as_str();
        assert!(
            ua.contains("AppleWebKit/537.36"),
            "missing AppleWebKit: {ua}"
        );
        assert!(ua.contains("Electron/"), "missing Electron: {ua}");
        assert!(ua.contains("Chrome/"), "missing Chrome: {ua}");
        assert!(ua.contains("Antigravity/"), "missing Antigravity: {ua}");
        assert!(ua.contains("Safari/537.36"), "missing Safari: {ua}");
    }

    #[test]
    fn test_user_agent_platform() {
        let ua = USER_AGENT.as_str();
        let os = std::env::consts::OS;
        match os {
            "macos" => assert!(ua.contains("Macintosh"), "wrong platform in UA for macOS"),
            "windows" => assert!(
                ua.contains("Windows NT"),
                "wrong platform in UA for Windows"
            ),
            _ => assert!(ua.contains("X11; Linux"), "wrong platform in UA for Linux"),
        }
    }

    #[test]
    fn test_session_id_is_uuid() {
        let sid = SESSION_ID.as_str();
        assert!(
            uuid::Uuid::parse_str(sid).is_ok(),
            "not a valid UUID: {sid}"
        );
    }

    #[test]
    fn test_session_id_stable() {
        let a = SESSION_ID.as_str();
        let b = SESSION_ID.as_str();
        assert_eq!(a, b, "SESSION_ID should be stable within a process");
    }

    #[test]
    fn test_version_is_semver() {
        let ver = VERSION.as_str();
        let parts: Vec<&str> = ver.split('.').collect();
        assert_eq!(parts.len(), 3, "version should be X.Y.Z: {ver}");
        for p in &parts {
            assert!(
                p.chars().all(|c| c.is_ascii_digit()),
                "non-digit in version part: {p}"
            );
        }
    }

    #[test]
    fn test_fingerprint_headers_complete() {
        let headers = build_fingerprint_headers();
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_ref()).collect();

        assert!(names.contains(&"User-Agent"), "missing User-Agent");
        assert!(names.contains(&"X-Client-Name"), "missing X-Client-Name");
        assert!(
            names.contains(&"X-Client-Version"),
            "missing X-Client-Version"
        );
        assert!(
            names.contains(&"X-VSCode-SessionId"),
            "missing X-VSCode-SessionId"
        );
        assert!(
            names.contains(&"X-Goog-Api-Client"),
            "missing X-Goog-Api-Client"
        );
        // X-Machine-Id is optional (may fail in sandboxed tests)
    }

    #[test]
    fn test_extract_semver_valid() {
        assert_eq!(extract_semver("1.16.5"), Some("1.16.5".to_string()));
        assert_eq!(extract_semver("Version: 2.0.0"), Some("2.0.0".to_string()));
        assert_eq!(extract_semver("v1.2.3 extra"), Some("1.2.3".to_string()));
        assert_eq!(
            extract_semver("1.107.0\n1504c8cc4b34d\nx64"),
            Some("1.107.0".to_string())
        );
    }

    #[test]
    fn test_extract_semver_invalid() {
        assert_eq!(extract_semver("no version here"), None);
        assert_eq!(extract_semver(""), None);
        assert_eq!(extract_semver("1.2"), None); // only X.Y, not X.Y.Z
    }
}
