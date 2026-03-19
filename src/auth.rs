use anyhow::{bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::persistence::rum_config_dir;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OAuthCredentials {
    pub access: String,
    pub refresh: String,
    // unix timestamp in milliseconds
    pub expires: u64,
}

pub fn auth_file_path() -> PathBuf {
    rum_config_dir().join("auth.json")
}

pub fn load_auth() -> Option<OAuthCredentials> {
    let content = std::fs::read_to_string(auth_file_path()).ok()?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&content).ok()?;
    serde_json::from_value(map.get("anthropic")?.clone()).ok()
}

pub fn save_auth(creds: &OAuthCredentials) -> Result<()> {
    let path = auth_file_path();
    std::fs::create_dir_all(path.parent().unwrap())?;

    let mut map: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    map.insert("anthropic".to_string(), serde_json::to_value(creds)?);

    let json = serde_json::to_string_pretty(&map)?;
    std::fs::write(&path, &json)?;

    // restrict to owner read/write on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

pub fn delete_auth() -> Result<()> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&content).unwrap_or_default();
    map.remove("anthropic");
    std::fs::write(&path, serde_json::to_string_pretty(&map)?)?;
    Ok(())
}

// refreshes the stored oauth token if it's expired or within 5 minutes of expiry.
// returns the new credentials if a refresh occurred.
pub async fn maybe_refresh() -> Option<OAuthCredentials> {
    let creds = load_auth()?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if now_ms + 5 * 60 * 1000 < creds.expires {
        return None;
    }
    let new_creds = refresh(&creds.refresh).await.ok()?;
    let _ = save_auth(&new_creds);
    Some(new_creds)
}

// generates (verifier, challenge) for the pkce flow
pub fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    (verifier, challenge)
}

// builds the authorization url and returns it along with the verifier to be
// kept for the token exchange step
pub fn build_auth_url() -> (String, String) {
    let (verifier, challenge) = generate_pkce();

    let url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        AUTHORIZE_URL,
        CLIENT_ID,
        percent_encode(REDIRECT_URI),
        percent_encode(SCOPES),
        challenge,
        verifier,
    );

    (url, verifier)
}

// parses the auth response pasted by the user. accepts either:
//   CODE#STATE  (the short form shown on the callback page)
//   https://console.anthropic.com/oauth/code/callback?code=CODE&state=STATE
pub fn parse_auth_response(input: &str) -> Option<(String, String)> {
    let input = input.trim();

    if !input.starts_with("http") {
        // short form: CODE#STATE
        let pos = input.find('#')?;
        let code = input[..pos].trim().to_string();
        let state = input[pos + 1..].trim().to_string();
        if !code.is_empty() && !state.is_empty() {
            return Some((code, state));
        }
        return None;
    }

    // full redirect url: parse query params manually
    let query = input.splitn(2, '?').nth(1)?;
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("");
        match key {
            "code" => code = Some(val.to_string()),
            "state" => state = Some(val.to_string()),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) if !c.is_empty() && !s.is_empty() => Some((c, s)),
        _ => None,
    }
}

pub async fn exchange_code(code: &str, state: &str, verifier: &str) -> Result<OAuthCredentials> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
    });

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("token exchange failed ({}): {}", status, text);
    }

    let data = resp.json::<TokenData>().await?;
    Ok(credentials_from_token_data(data))
}

pub async fn refresh(refresh_token: &str) -> Result<OAuthCredentials> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("token refresh failed ({}): {}", status, text);
    }

    let data = resp.json::<TokenData>().await?;
    Ok(credentials_from_token_data(data))
}

#[derive(Deserialize)]
struct TokenData {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

fn credentials_from_token_data(data: TokenData) -> OAuthCredentials {
    // 5 minute buffer before the stated expiry
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        + data.expires_in * 1000
        - 5 * 60 * 1000;

    OAuthCredentials {
        access: data.access_token,
        refresh: data.refresh_token,
        expires,
    }
}

// tries to open the url in the system browser
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b => {
                out.push('%');
                out.push(
                    char::from_digit((b >> 4) as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                out.push(
                    char::from_digit((b & 0xf) as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    out
}
