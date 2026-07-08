use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{AppUsageData, UsageData, UsageSection};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CREATE_NO_WINDOW: u32 = 0x08000000;

const MODEL_FALLBACK_CHAIN: &[&str] = &["claude-3-haiku-20240307", "claude-haiku-4-5-20251001"];

#[derive(Debug)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ActiveSource,
    AllSources,
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

impl Drop for CodexTokenData {
    fn drop(&mut self) {
        zero_string(&mut self.access_token);
    }
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: i64,
}

pub fn poll(show_claude_code: bool, show_codex: bool) -> Result<AppUsageData, PollError> {
    let mut data = AppUsageData::default();

    if show_claude_code {
        data.claude_code = Some(poll_claude_code()?);
    }

    if show_codex {
        match poll_codex() {
            Ok(codex) => data.codex = Some(codex),
            Err(error) if !show_claude_code => return Err(error),
            Err(error) => diagnose::log(format!("Codex usage poll failed: {error:?}")),
        }
    }

    if data.claude_code.is_none() && data.codex.is_none() {
        Err(PollError::RequestFailed)
    } else {
        Ok(data)
    }
}

fn poll_claude_code() -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log("poll failed: no Claude credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let creds = refresh_or_fallback(creds)?;

    fetch_usage_with_fallback(&creds.access_token)
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    match fetch_codex_usage(&creds.access_token, creds.account_id.as_deref()) {
        Ok(data) => Ok(data),
        Err(PollError::AuthRequired) => {
            cli_refresh_codex_token();
            let refreshed = read_codex_credentials().ok_or(PollError::TokenExpired)?;
            fetch_codex_usage(&refreshed.access_token, refreshed.account_id.as_deref())
        }
        Err(error) => Err(error),
    }
}

fn refresh_or_fallback(mut creds: Credentials) -> Result<Credentials, PollError> {
    loop {
        if !is_token_expired(creds.expires_at) {
            return Ok(creds);
        }

        let source = creds.source.clone();
        cli_refresh_token(&source);

        match read_credentials_from_source(&source) {
            Some(refreshed) if !is_token_expired(refreshed.expires_at) => return Ok(refreshed),
            Some(_) => diagnose::log(format!(
                "credentials from {source:?} still expired after refresh attempt"
            )),
            None => diagnose::log(format!(
                "credentials from {source:?} unavailable after refresh attempt"
            )),
        }

        match read_next_credentials_after(&source) {
            Some(next) => creds = next,
            None => return Err(PollError::TokenExpired),
        }
    }
}

/// Invoke the Claude CLI with a minimal prompt to force its internal
/// OAuth token refresh.
fn cli_refresh_token(source: &CredentialSource) {
    match source {
        CredentialSource::Windows(_) => cli_refresh_windows_token(),
        CredentialSource::Wsl { distro } => cli_refresh_wsl_token(distro),
    }
}

fn cli_refresh_windows_token() {
    let claude_path = resolve_windows_claude_path();
    let is_cmd = claude_path.to_lowercase().ends_with(".cmd");
    diagnose::log(format!(
        "attempting Windows Claude token refresh via {claude_path}"
    ));

    let args: &[&str] = &["-p", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&claude_path).args(args);
        c
    } else {
        let mut c = Command::new(&claude_path);
        c.args(args);
        c
    };
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Claude token refresh", error);
            return;
        }
    };

    // Wait up to 30 seconds — don't block the poll thread forever
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

fn cli_refresh_wsl_token(distro: &str) {
    diagnose::log(format!(
        "attempting WSL Claude token refresh in distro {distro}"
    ));
    let mut cmd = Command::new("wsl.exe");
    cmd.arg("-d")
        .arg(distro)
        .arg("--")
        .arg("bash")
        .arg("-lic")
        .arg("if command -v claude >/dev/null 2>&1; then claude -p .; elif [ -x \"$HOME/.local/bin/claude\" ]; then \"$HOME/.local/bin/claude\" -p .; else exit 127; fi")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn WSL Claude token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

fn cli_refresh_codex_token() {
    let codex_path = resolve_windows_codex_path();
    let is_cmd = codex_path.to_lowercase().ends_with(".cmd");
    let is_ps1 = codex_path.to_lowercase().ends_with(".ps1");
    diagnose::log(format!(
        "attempting Windows Codex token refresh via {codex_path}"
    ));

    let args: &[&str] = &["exec", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&codex_path).args(args);
        c
    } else if is_ps1 {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&codex_path)
            .args(args);
        c
    } else {
        let mut c = Command::new(&codex_path);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Codex token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_refresh(child: &mut std::process::Child) {
    // Wait up to 30 seconds; don't block the poll thread forever.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

/// Resolve the full path to the `claude` CLI executable.
fn resolve_windows_claude_path() -> String {
    for name in &["claude.cmd", "claude"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["claude.cmd", "claude"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "claude.cmd".to_string()
}

fn resolve_windows_codex_path() -> String {
    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "codex.cmd".to_string()
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    let sources = match mode {
        CredentialWatchMode::ActiveSource => read_first_credentials()
            .map(|creds| vec![creds.source.clone()])
            .unwrap_or_else(all_known_credential_sources),
        CredentialWatchMode::AllSources => all_known_credential_sources(),
    };

    let mut snapshot: CredentialWatchSnapshot = sources
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect();
    snapshot.sort();
    snapshot.dedup();
    snapshot
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    let mut sources = Vec::new();
    if let Some(source) = windows_credential_source() {
        sources.push(source);
    }
    for distro in list_wsl_distros() {
        sources.push(CredentialSource::Wsl { distro });
    }
    sources
}

fn windows_credential_source() -> Option<CredentialSource> {
    let home = dirs::home_dir()?;
    Some(CredentialSource::Windows(
        home.join(".claude").join(".credentials.json"),
    ))
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    }
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(
                "if [ -f ~/.claude/.credentials.json ]; then \
                 stat -c 'present|%s|%Y' ~/.claude/.credentials.json; \
                 else echo missing; fi",
            )
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    let state = if output.status.success() {
        decode_wsl_text(&output.stdout).trim().to_string()
    } else {
        format!("status-{}", output.status)
    };

    Some(format!("wsl:{distro}|{state}"))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    // Try the dedicated usage endpoint first
    match try_usage_endpoint(token)? {
        Some(data) => {
            // If reset timers are missing, fill them in from the Messages API
            if data.session.resets_at.is_none() || data.weekly.resets_at.is_none() {
                if let Ok(fallback) = fetch_usage_via_messages(token) {
                    let mut merged = data;
                    if merged.session.resets_at.is_none() {
                        merged.session.resets_at = fallback.session.resets_at;
                    }
                    if merged.weekly.resets_at.is_none() {
                        merged.weekly.resets_at = fallback.weekly.resets_at;
                    }
                    return Ok(merged);
                }
            }
            return Ok(data);
        }
        None => {}
    }

    // Fall back to Messages API with rate limit headers
    let result = fetch_usage_via_messages(token);
    if result.is_err() {
        diagnose::log("usage endpoint and Messages API fallback both failed");
    }
    result
}

fn try_usage_endpoint(token: &str) -> Result<Option<UsageData>, PollError> {
    let agent = build_agent()?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "usage endpoint returned auth error status {code}; re-login required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(_) => return Ok(None),
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    let mut data = UsageData::default();

    if let Some(bucket) = &response.five_hour {
        data.session.percentage = bucket.utilization;
        data.session.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    if let Some(bucket) = &response.seven_day {
        data.weekly.percentage = bucket.utilization;
        data.weekly.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    Ok(Some(data))
}

fn fetch_usage_via_messages(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    for model in MODEL_FALLBACK_CHAIN {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}]
        });

        let response = match agent
            .post(MESSAGES_URL)
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "oauth-2025-04-20")
            .send_json(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
                diagnose::log(format!(
                    "messages endpoint returned auth error status {code}; re-login required"
                ));
                return Err(PollError::AuthRequired);
            }
            Err(ureq::Error::Status(_code, resp)) => resp,
            Err(_) => continue,
        };

        let h5 = response.header("anthropic-ratelimit-unified-5h-utilization");
        let h7 = response.header("anthropic-ratelimit-unified-7d-utilization");
        let hs = response.header("anthropic-ratelimit-unified-status");

        if h5.is_some() || h7.is_some() || hs.is_some() {
            return Ok(parse_rate_limit_headers(&response));
        }
    }

    Err(PollError::RequestFailed)
}

fn clamp_utilization(raw: f64) -> f64 {
    if raw.is_nan() || raw.is_infinite() || raw < 0.0 {
        return 0.0;
    }
    raw.min(1.0)
}

fn parse_rate_limit_headers(response: &ureq::Response) -> UsageData {
    let mut data = UsageData::default();

    data.session.percentage =
        clamp_utilization(get_header_f64(response, "anthropic-ratelimit-unified-5h-utilization"))
            * 100.0;
    data.session.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-5h-reset",
    ));

    data.weekly.percentage =
        clamp_utilization(get_header_f64(response, "anthropic-ratelimit-unified-7d-utilization"))
            * 100.0;
    data.weekly.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-7d-reset",
    ));

    let overall_reset = get_header_i64(response, "anthropic-ratelimit-unified-reset");

    if data.session.percentage == 0.0 && data.weekly.percentage == 0.0 {
        let status = response.header("anthropic-ratelimit-unified-status");
        if status == Some("rejected") {
            let claim = response.header("anthropic-ratelimit-unified-representative-claim");
            match claim {
                Some("five_hour") => data.session.percentage = 100.0,
                Some("seven_day") => data.weekly.percentage = 100.0,
                _ => {}
            }
        }

        if data.session.resets_at.is_none() && overall_reset.is_some() {
            data.session.resets_at = unix_to_system_time(overall_reset);
        }
    }

    data
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Codex usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexUsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    codex_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = *response.rate_limit.flatten()?;
    let mut data = UsageData::default();

    if let Some(window) = details.primary_window.flatten() {
        data.session = codex_section_from_window(&window);
    }

    if let Some(window) = details.secondary_window.flatten() {
        data.weekly = codex_section_from_window(&window);
    }

    Some(data)
}

fn codex_section_from_window(window: &CodexRateLimitWindow) -> UsageSection {
    UsageSection {
        percentage: window.used_percent,
        resets_at: unix_to_system_time(Some(window.reset_at)),
    }
}

fn get_header_f64(response: &ureq::Response, name: &str) -> f64 {
    response
        .header(name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn get_header_i64(response: &ureq::Response, name: &str) -> Option<i64> {
    response.header(name).and_then(|s| s.parse::<i64>().ok())
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    // Reject timestamps beyond year 2200 — clearly invalid API data.
    if secs > 7_258_118_400 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
}

/// Overwrite `s`'s heap allocation with zeros before it is freed so the bytes
/// cannot be recovered from a subsequent heap or process memory dump.
/// `write_volatile` prevents the compiler from treating this as dead-store and
/// optimising it away.
fn zero_string(s: &mut String) {
    // SAFETY: We hold `&mut String` so there is no aliasing. The bytes remain
    // allocated and valid until after this function returns and String::drop
    // calls the allocator.
    let bytes = unsafe { s.as_bytes_mut() };
    for b in bytes.iter_mut() {
        unsafe { std::ptr::write_volatile(b as *mut u8, 0u8) };
    }
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        zero_string(&mut self.access_token);
    }
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
}

fn read_first_credentials() -> Option<Credentials> {
    if let Some(creds) = read_windows_credentials() {
        return Some(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            return Some(creds);
        }
    }

    None
}

fn path_is_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn read_windows_credentials() -> Option<Credentials> {
    let CredentialSource::Windows(cred_path) = windows_credential_source()? else {
        return None;
    };

    // Refuse to follow symlinks — a symlink could redirect reads to an
    // attacker-controlled location, or be used to exfiltrate credentials.
    if path_is_symlink(&cred_path) || path_is_symlink(cred_path.parent().unwrap_or(&cred_path)) {
        diagnose::log(format!(
            "refusing to read credentials: symlink detected at {}",
            cred_path.display()
        ));
        return None;
    }

    let content = match std::fs::read_to_string(&cred_path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows credentials at {}",
                        cred_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => {
            let content = std::fs::read_to_string(path).ok()?;
            parse_credentials(&content, source.clone())
        }
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(codex_home.join("auth.json"));
    }

    Some(dirs::home_dir()?.join(".codex").join("auth.json"))
}

fn read_codex_credentials() -> Option<CodexTokenData> {
    let auth_path = codex_auth_path()?;
    let content = match std::fs::read_to_string(&auth_path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!(
                    "unable to read Codex credentials at {}",
                    auth_path.display()
                ),
                error,
            );
            return None;
        }
    };

    let auth: CodexAuthFile = serde_json::from_str(&content).ok()?;
    auth.tokens.filter(|tokens| !tokens.access_token.is_empty())
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    // Reject obviously invalid tokens
    if access_token.is_empty() || access_token.len() > 8192 {
        diagnose::log("credential file contains an invalid access token (unexpected length)");
        return None;
    }
    if !access_token.chars().all(|c| c.is_ascii() && !c.is_ascii_control()) {
        diagnose::log("credential file contains an access token with unexpected characters");
        return None;
    }

    Some(Credentials {
        access_token,
        expires_at,
        source,
    })
}

fn read_next_credentials_after(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(_) => {
            for distro in list_wsl_distros() {
                if let Some(creds) = read_wsl_credentials(&distro) {
                    return Some(creds);
                }
            }
        }
        CredentialSource::Wsl { distro } => {
            let mut past_current = false;
            for candidate_distro in list_wsl_distros() {
                if !past_current {
                    past_current = candidate_distro == *distro;
                    continue;
                }
                if let Some(creds) = read_wsl_credentials(&candidate_distro) {
                    return Some(creds);
                }
            }
        }
    }

    None
}

fn is_safe_wsl_distro_name(name: &str) -> bool {
    // Distro names from WSL should only contain alphanumerics, spaces, hyphens,
    // underscores, and dots. Reject anything that looks like shell metacharacters.
    !name.is_empty()
        && name.len() <= 256
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|name| {
            if is_safe_wsl_distro_name(name) {
                true
            } else {
                diagnose::log(format!("skipping WSL distro with unexpected name: {name:?}"));
                false
            }
        })
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?;
    // Strip timezone offset to get "YYYY-MM-DDTHH:MM:SS" or with fractional seconds
    // The API returns formats like "2026-03-05T08:00:00.321598+00:00"
    let datetime_part = s.split('+').next().unwrap_or(s);
    let datetime_part = datetime_part.split('Z').next().unwrap_or(datetime_part);

    // Try parsing with and without fractional seconds
    let formats = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"];
    for fmt in &formats {
        if let Ok(secs) = parse_datetime_to_unix(datetime_part, fmt) {
            return Some(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
    None
}

/// Minimal datetime parser — avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str, _fmt: &str) -> Result<u64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;

    if !(1970..=9999).contains(&year) {
        return Err(());
    }
    if !(1..=12).contains(&month) {
        return Err(());
    }
    if !(1..=31).contains(&day) {
        return Err(());
    }

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;

    if hour > 23 || min > 59 || sec > 60 {
        return Err(());
    }

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X% · Yh" style text
pub fn format_line(section: &UsageSection, strings: Strings) -> String {
    let pct = format!("{:.0}%", section.percentage);
    let cd = format_countdown(section.resets_at, strings);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct} \u{00b7} {cd}")
    }
}

fn format_countdown(resets_at: Option<SystemTime>, strings: Strings) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return strings.now.to_string(),
    };

    format_countdown_from_secs(remaining.as_secs(), strings)
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64, strings: Strings) -> String {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        format!("{total_days}{}", strings.day_suffix)
    } else if total_hours >= 1 {
        format!("{total_hours}{}", strings.hour_suffix)
    } else if total_mins >= 1 {
        format!("{total_mins}{}", strings.minute_suffix)
    } else {
        format!("{total_secs}{}", strings.second_suffix)
    }
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        total_days * 86400
    } else if total_hours >= 1 {
        total_hours * 3600
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if either section has reached "now" (reset time has passed).
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    past(&data.session) || past(&data.weekly)
}

pub fn app_is_past_reset(data: &AppUsageData) -> bool {
    data.claude_code.as_ref().is_some_and(is_past_reset)
        || data.codex.as_ref().is_some_and(is_past_reset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localization::LanguageId;

    fn en() -> Strings {
        LanguageId::English.strings()
    }

    // -- clamp_utilization --

    #[test]
    fn clamp_utilization_passes_through_normal_values() {
        assert_eq!(clamp_utilization(0.0), 0.0);
        assert_eq!(clamp_utilization(0.5), 0.5);
        assert_eq!(clamp_utilization(1.0), 1.0);
    }

    #[test]
    fn clamp_utilization_clamps_above_one() {
        assert_eq!(clamp_utilization(1.5), 1.0);
        assert_eq!(clamp_utilization(f64::INFINITY), 0.0);
    }

    #[test]
    fn clamp_utilization_floors_negative_and_nan() {
        assert_eq!(clamp_utilization(-0.5), 0.0);
        assert_eq!(clamp_utilization(f64::NAN), 0.0);
    }

    // -- parse_iso8601 / parse_datetime_to_unix --

    #[test]
    fn parse_iso8601_returns_none_for_missing_input() {
        assert!(parse_iso8601(None).is_none());
    }

    #[test]
    fn parse_iso8601_parses_utc_zulu_timestamp() {
        let parsed = parse_iso8601(Some("2026-03-05T08:00:00Z")).unwrap();
        assert_eq!(
            parsed.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1772697600
        );
    }

    #[test]
    fn parse_iso8601_parses_offset_timestamp_with_fractional_seconds() {
        let parsed = parse_iso8601(Some("2026-03-05T08:00:00.321598+00:00")).unwrap();
        assert_eq!(
            parsed.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1772697600
        );
    }

    #[test]
    fn parse_iso8601_rejects_malformed_input() {
        assert!(parse_iso8601(Some("not-a-date")).is_none());
        assert!(parse_iso8601(Some("2026-13-40T99:99:99")).is_none());
    }

    #[test]
    fn parse_datetime_to_unix_matches_known_epoch_values() {
        assert_eq!(parse_datetime_to_unix("1970-01-01T00:00:00", ""), Ok(0));
        // 2000-03-01 accounts for the Feb 29 2000 leap day correctly.
        assert_eq!(
            parse_datetime_to_unix("2000-03-01T00:00:00", ""),
            Ok(951868800)
        );
    }

    // -- is_leap --

    #[test]
    fn is_leap_identifies_leap_years() {
        assert!(is_leap(2000));
        assert!(is_leap(2024));
        assert!(!is_leap(1900));
        assert!(!is_leap(2023));
    }

    // -- unix_to_system_time --

    #[test]
    fn unix_to_system_time_converts_valid_timestamp() {
        let t = unix_to_system_time(Some(1000)).unwrap();
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_secs(), 1000);
    }

    #[test]
    fn unix_to_system_time_rejects_none_negative_and_far_future() {
        assert!(unix_to_system_time(None).is_none());
        assert!(unix_to_system_time(Some(-1)).is_none());
        assert!(unix_to_system_time(Some(7_258_118_401)).is_none());
    }

    // -- is_token_expired --

    #[test]
    fn is_token_expired_true_for_past_timestamp() {
        assert!(is_token_expired(Some(1)));
    }

    #[test]
    fn is_token_expired_false_for_far_future_timestamp() {
        let far_future_ms = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64)
            + 60_000;
        assert!(!is_token_expired(Some(far_future_ms)));
    }

    #[test]
    fn is_token_expired_false_when_missing() {
        assert!(!is_token_expired(None));
    }

    // -- format_countdown_from_secs / time_until_display_change_from_secs --

    #[test]
    fn format_countdown_from_secs_picks_largest_unit() {
        assert_eq!(format_countdown_from_secs(30, en()), format!("30{}", en().second_suffix));
        assert_eq!(format_countdown_from_secs(90, en()), format!("1{}", en().minute_suffix));
        assert_eq!(format_countdown_from_secs(3700, en()), format!("1{}", en().hour_suffix));
        assert_eq!(format_countdown_from_secs(90_000, en()), format!("1{}", en().day_suffix));
    }

    #[test]
    fn time_until_display_change_from_secs_ticks_to_next_bucket_boundary() {
        // 90 seconds -> currently showing "1 minute", next change is when the
        // minute count changes at 120s, i.e. in 30s (+1 for rounding safety).
        assert_eq!(
            time_until_display_change_from_secs(90),
            Duration::from_secs(31)
        );
        // Under 60s counts down second by second.
        assert_eq!(
            time_until_display_change_from_secs(45),
            Duration::from_secs(1)
        );
    }

    // -- is_past_reset / app_is_past_reset --

    #[test]
    fn is_past_reset_true_when_reset_time_has_elapsed() {
        let mut data = UsageData::default();
        data.session.resets_at = Some(SystemTime::now() - Duration::from_secs(5));
        assert!(is_past_reset(&data));
    }

    #[test]
    fn is_past_reset_false_when_reset_time_is_future_or_absent() {
        let mut data = UsageData::default();
        data.session.resets_at = Some(SystemTime::now() + Duration::from_secs(3600));
        assert!(!is_past_reset(&data));

        let empty = UsageData::default();
        assert!(!is_past_reset(&empty));
    }

    #[test]
    fn app_is_past_reset_checks_both_apps() {
        let mut past = UsageData::default();
        past.weekly.resets_at = Some(SystemTime::now() - Duration::from_secs(1));

        let data = AppUsageData {
            codex: Some(past),
            ..Default::default()
        };
        assert!(app_is_past_reset(&data));

        let empty = AppUsageData::default();
        assert!(!app_is_past_reset(&empty));
    }

    // -- codex_usage_from_response / codex_section_from_window --

    #[test]
    fn codex_usage_from_response_maps_windows_to_sections() {
        let response = CodexUsageResponse {
            rate_limit: Some(Some(Box::new(CodexRateLimitDetails {
                primary_window: Some(Some(Box::new(CodexRateLimitWindow {
                    used_percent: 12.5,
                    reset_at: 1000,
                }))),
                secondary_window: Some(Some(Box::new(CodexRateLimitWindow {
                    used_percent: 60.0,
                    reset_at: 2000,
                }))),
            }))),
        };

        let data = codex_usage_from_response(response).unwrap();
        assert_eq!(data.session.percentage, 12.5);
        assert_eq!(
            data.session.resets_at.unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1000
        );
        assert_eq!(data.weekly.percentage, 60.0);
    }

    #[test]
    fn codex_usage_from_response_none_when_rate_limit_missing() {
        let response = CodexUsageResponse { rate_limit: None };
        assert!(codex_usage_from_response(response).is_none());

        let response_null = CodexUsageResponse {
            rate_limit: Some(None),
        };
        assert!(codex_usage_from_response(response_null).is_none());
    }

    #[test]
    fn codex_usage_from_response_defaults_missing_windows() {
        let response = CodexUsageResponse {
            rate_limit: Some(Some(Box::new(CodexRateLimitDetails {
                primary_window: None,
                secondary_window: None,
            }))),
        };
        let data = codex_usage_from_response(response).unwrap();
        assert_eq!(data.session.percentage, 0.0);
        assert_eq!(data.weekly.percentage, 0.0);
    }

    // -- is_safe_wsl_distro_name --

    #[test]
    fn is_safe_wsl_distro_name_accepts_typical_names() {
        assert!(is_safe_wsl_distro_name("Ubuntu-22.04"));
        assert!(is_safe_wsl_distro_name("Debian GNU_Linux"));
    }

    #[test]
    fn is_safe_wsl_distro_name_rejects_empty_or_shell_metacharacters() {
        assert!(!is_safe_wsl_distro_name(""));
        assert!(!is_safe_wsl_distro_name("Ubuntu; rm -rf /"));
        assert!(!is_safe_wsl_distro_name("$(whoami)"));
        assert!(!is_safe_wsl_distro_name(&"a".repeat(257)));
    }

    // -- decode_utf16le / looks_like_utf16le / decode_wsl_text --

    #[test]
    fn decode_wsl_text_decodes_utf16le_with_bom() {
        // 0xFF 0xFE is the little-endian UTF-16 BOM, followed by "hi" as UTF-16LE code units.
        let mut bytes = vec![0xFF, 0xFE];
        for ch in "hi".encode_utf16() {
            bytes.extend_from_slice(&ch.to_le_bytes());
        }
        assert_eq!(decode_wsl_text(&bytes), "hi");
    }

    #[test]
    fn decode_wsl_text_falls_back_to_utf8_for_plain_text() {
        assert_eq!(decode_wsl_text(b"present|123|456"), "present|123|456");
    }

    #[test]
    fn decode_wsl_text_handles_empty_input() {
        assert_eq!(decode_wsl_text(b""), "");
    }

    // -- parse_credentials --

    #[test]
    fn parse_credentials_extracts_token_and_expiry() {
        let json = r#"{"claudeAiOauth":{"accessToken":"abc123","expiresAt":9999999999}}"#;
        let creds = parse_credentials(
            json,
            CredentialSource::Windows(PathBuf::from("C:\\creds.json")),
        )
        .unwrap();
        assert_eq!(creds.access_token, "abc123");
        assert_eq!(creds.expires_at, Some(9999999999));
    }

    #[test]
    fn parse_credentials_rejects_missing_oauth_block() {
        assert!(parse_credentials(
            "{}",
            CredentialSource::Windows(PathBuf::from("C:\\creds.json"))
        )
        .is_none());
    }

    #[test]
    fn parse_credentials_rejects_invalid_json() {
        assert!(parse_credentials(
            "not json",
            CredentialSource::Windows(PathBuf::from("C:\\creds.json"))
        )
        .is_none());
    }

    #[test]
    fn parse_credentials_rejects_empty_token() {
        let json = r#"{"claudeAiOauth":{"accessToken":""}}"#;
        assert!(parse_credentials(
            json,
            CredentialSource::Windows(PathBuf::from("C:\\creds.json"))
        )
        .is_none());
    }

    #[test]
    fn parse_credentials_rejects_control_characters_in_token() {
        let json = "{\"claudeAiOauth\":{\"accessToken\":\"abc\\u0007def\"}}";
        assert!(parse_credentials(
            json,
            CredentialSource::Windows(PathBuf::from("C:\\creds.json"))
        )
        .is_none());
    }
}
