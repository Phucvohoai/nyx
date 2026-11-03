use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    future::Future,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};

use argon2::Argon2;
use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use cookie_store::CookieStore;
use rand_core::RngCore;
use reqwest::{
    cookie::CookieStore as ReqwestCookieStore,
    header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT},
    Client,
};
use thiserror::Error;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdapterError>> + Send + 'a>>;

const VAULT_MAGIC: &[u8; 8] = b"NYXSESS1";
const VAULT_SALT_LEN: usize = 16;
const VAULT_NONCE_LEN: usize = 24;
const VAULT_KEY_LEN: usize = 32;
const DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
const SEARCH_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60 * 60);
const MAX_IMAGE_RESPONSE_BYTES: usize = 20 * 1024 * 1024;
type DecryptedVault = (
    [u8; VAULT_SALT_LEN],
    Zeroizing<[u8; VAULT_KEY_LEN]>,
    Zeroizing<Vec<u8>>,
);

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("image response exceeded the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
    #[error("search provider rate limited requests; retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    #[error("invalid response cookie: {0}")]
    CookieStore(String),
    #[error("session vault I/O failed: {0}")]
    VaultIo(#[from] std::io::Error),
    #[error("session vault error: {0}")]
    Vault(String),
    #[error("session vault format is invalid or its passphrase is incorrect")]
    VaultAuthentication,
}

impl AdapterError {
    pub fn is_forbidden(&self) -> bool {
        matches!(self, Self::Http(error) if error.status() == Some(reqwest::StatusCode::FORBIDDEN))
    }

    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SystemMetrics {
    pub ram_used_bytes: Option<u64>,
    pub ram_total_bytes: Option<u64>,
    pub zpool_lz4_ratio: Option<f64>,
    pub active_core: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormRequestMethod {
    Get,
    Post,
}

#[derive(Debug, PartialEq, Eq)]
pub struct FormSubmission {
    pub url: String,
    pub method: FormRequestMethod,
    pub fields: Vec<(String, String)>,
}

impl Drop for FormSubmission {
    fn drop(&mut self) {
        self.fields
            .iter_mut()
            .for_each(|(_, value)| value.zeroize());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageResponse {
    pub url: Url,
    pub body: String,
}

pub trait NetworkAdapter: Send + Sync {
    fn fetch<'a>(&'a self, url: &'a str) -> AdapterFuture<'a, String>;
    fn fetch_bytes<'a>(&'a self, url: &'a str) -> AdapterFuture<'a, Vec<u8>> {
        Box::pin(async move { self.fetch(url).await.map(String::into_bytes) })
    }
    fn fetch_page<'a>(&'a self, url: &'a str) -> AdapterFuture<'a, PageResponse>;
    fn submit_form<'a>(&'a self, submission: &'a FormSubmission)
        -> AdapterFuture<'a, PageResponse>;
}

pub trait SystemAdapter: Send + Sync {
    fn metrics(&self) -> Result<SystemMetrics, AdapterError>;
}

#[derive(Debug, Default)]
struct SearchRateLimiter {
    hosts: Mutex<HashMap<String, SearchHostSchedule>>,
}

#[derive(Debug, Default)]
struct SearchHostSchedule {
    next_request: Option<Instant>,
    cooldown_until: Option<Instant>,
}

impl SearchRateLimiter {
    async fn wait(&self, host: &str) {
        loop {
            let delay = {
                let mut hosts = match self.hosts.lock() {
                    Ok(hosts) => hosts,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let now = Instant::now();
                let schedule = hosts.entry(host.to_owned()).or_default();
                let allowed_at = schedule
                    .next_request
                    .into_iter()
                    .chain(schedule.cooldown_until)
                    .fold(now, Instant::max);
                schedule.next_request = Some(allowed_at + SEARCH_REQUEST_INTERVAL);
                allowed_at.saturating_duration_since(now)
            };
            tokio::time::sleep(delay).await;

            let cooldown_active = {
                let hosts = match self.hosts.lock() {
                    Ok(hosts) => hosts,
                    Err(poisoned) => poisoned.into_inner(),
                };
                hosts
                    .get(host)
                    .and_then(|schedule| schedule.cooldown_until)
                    .is_some_and(|deadline| deadline > Instant::now())
            };
            if !cooldown_active {
                return;
            }
        }
    }

    fn cool_down(&self, host: &str, duration: Duration) {
        let mut hosts = match self.hosts.lock() {
            Ok(hosts) => hosts,
            Err(poisoned) => poisoned.into_inner(),
        };
        let schedule = hosts.entry(host.to_owned()).or_default();
        let cooldown_until = Instant::now() + duration;
        schedule.cooldown_until = Some(
            schedule
                .cooldown_until
                .map_or(cooldown_until, |deadline| deadline.max(cooldown_until)),
        );
    }
}

#[derive(Debug)]
struct NyxCookieJar {
    store: RwLock<CookieStore>,
    last_error: Mutex<Option<String>>,
}

impl NyxCookieJar {
    fn new(store: CookieStore) -> Self {
        Self {
            store: RwLock::new(store),
            last_error: Mutex::new(None),
        }
    }

    fn record_error(&self, message: String) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = Some(message);
        }
    }

    fn take_error(&self) -> Option<String> {
        match self.last_error.lock() {
            Ok(mut last_error) => last_error.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }
}

impl ReqwestCookieStore for NyxCookieJar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        let mut store = match self.store.write() {
            Ok(store) => store,
            Err(poisoned) => {
                self.record_error("cookie store lock was poisoned".to_owned());
                poisoned.into_inner()
            }
        };
        for header in headers {
            match header.to_str() {
                Ok(cookie) => {
                    if store.parse(cookie, url).is_err() {
                        self.record_error("server sent a malformed Set-Cookie header".to_owned());
                    }
                }
                Err(_) => {
                    self.record_error("server sent a non-text Set-Cookie header".to_owned());
                }
            }
        }
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        let store = match self.store.read() {
            Ok(store) => store,
            Err(poisoned) => {
                self.record_error("cookie store lock was poisoned".to_owned());
                poisoned.into_inner()
            }
        };
        let value = store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        (!value.is_empty())
            .then(|| HeaderValue::from_str(&value).ok())
            .flatten()
    }
}

#[derive(Debug)]
struct SessionVault {
    path: PathBuf,
    salt: [u8; VAULT_SALT_LEN],
    key: Zeroizing<[u8; VAULT_KEY_LEN]>,
}

#[derive(Debug, Clone)]
pub struct StdBackend {
    client: Client,
    cookie_jar: Arc<NyxCookieJar>,
    vault: Option<Arc<SessionVault>>,
    search_rate_limiter: Arc<SearchRateLimiter>,
}

impl StdBackend {
    pub fn new() -> Result<Self, AdapterError> {
        Self::with_store(CookieStore::default(), None)
    }

    pub fn open_session_vault(
        path: impl Into<PathBuf>,
        passphrase: &str,
    ) -> Result<(Self, bool), AdapterError> {
        let path = path.into();
        let is_new = !path.exists();
        let (store, salt, key) = if is_new {
            let mut salt = [0; VAULT_SALT_LEN];
            OsRng.fill_bytes(&mut salt);
            (
                CookieStore::default(),
                salt,
                derive_vault_key(passphrase, &salt)?,
            )
        } else {
            let bytes = fs::read(&path)?;
            let (salt, key, plaintext) = decrypt_vault(&bytes, passphrase)?;
            let store = cookie_store::serde::json::load_all(Cursor::new(&*plaintext))
                .map_err(|error| AdapterError::Vault(error.to_string()))?;
            (store, salt, key)
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let vault = Arc::new(SessionVault { path, salt, key });
        Ok((Self::with_store(store, Some(vault))?, is_new))
    }

    pub fn default_session_vault_path() -> Result<PathBuf, AdapterError> {
        #[cfg(windows)]
        let data_dir = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| AdapterError::Vault("APPDATA is not set".to_owned()))?;

        #[cfg(not(windows))]
        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .ok_or_else(|| {
                AdapterError::Vault("neither XDG_DATA_HOME nor HOME is set".to_owned())
            })?;

        Ok(data_dir.join("nyx").join("sessions.vault"))
    }

    pub fn save_session(&self) -> Result<(), AdapterError> {
        let Some(vault) = &self.vault else {
            return Ok(());
        };

        let store = self
            .cookie_jar
            .store
            .read()
            .map_err(|_| AdapterError::Vault("cookie store lock is poisoned".to_owned()))?;
        let mut plaintext = Zeroizing::new(Vec::new());
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut *plaintext)
            .map_err(|error| AdapterError::Vault(error.to_string()))?;
        let encrypted = encrypt_vault(&vault.salt, &vault.key, &plaintext)?;
        save_atomically(&vault.path, &encrypted)?;
        Ok(())
    }

    fn with_store(
        store: CookieStore,
        vault: Option<Arc<SessionVault>>,
    ) -> Result<Self, AdapterError> {
        let cookie_jar = Arc::new(NyxCookieJar::new(store));
        let mut default_headers = HeaderMap::new();
        default_headers.insert(USER_AGENT, HeaderValue::from_static(DESKTOP_USER_AGENT));
        default_headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
            ),
        );
        default_headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        default_headers.insert(
            "sec-ch-ua",
            HeaderValue::from_static(
                "\"Chromium\";v=\"122\", \"Google Chrome\";v=\"122\", \"Not(A:Brand\";v=\"24\"",
            ),
        );
        default_headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
        default_headers.insert(
            "sec-ch-ua-platform",
            HeaderValue::from_static("\"Windows\""),
        );
        let client = Client::builder()
            .default_headers(default_headers)
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.url().scheme() == "http"
                    && attempt
                        .previous()
                        .last()
                        .is_some_and(|url| url.scheme() == "https")
                {
                    attempt.error("refusing HTTPS-to-HTTP redirect")
                } else {
                    attempt.follow()
                }
            }))
            .cookie_provider(Arc::clone(&cookie_jar))
            .build()?;
        Ok(Self {
            client,
            cookie_jar,
            vault,
            search_rate_limiter: Arc::new(SearchRateLimiter::default()),
        })
    }
}

impl NetworkAdapter for StdBackend {
    fn fetch<'a>(&'a self, url: &'a str) -> AdapterFuture<'a, String> {
        Box::pin(async move {
            let parsed_url =
                Url::parse(url).map_err(|error| AdapterError::InvalidUrl(error.to_string()))?;
            let origin = parsed_url.origin().ascii_serialization();
            self.search_rate_limiter.wait(&origin).await;
            let request = self.client.get(parsed_url);
            Ok(self.send_request(request).await?.body)
        })
    }

    fn fetch_page<'a>(&'a self, url: &'a str) -> AdapterFuture<'a, PageResponse> {
        Box::pin(async move { self.send_request(self.client.get(url)).await })
    }

    fn fetch_bytes<'a>(&'a self, url: &'a str) -> AdapterFuture<'a, Vec<u8>> {
        Box::pin(async move { self.send_bytes_request(self.client.get(url)).await })
    }

    fn submit_form<'a>(
        &'a self,
        submission: &'a FormSubmission,
    ) -> AdapterFuture<'a, PageResponse> {
        Box::pin(async move {
            let request = match submission.method {
                FormRequestMethod::Get => {
                    let mut url = Url::parse(&submission.url)
                        .map_err(|error| AdapterError::InvalidUrl(error.to_string()))?;
                    url.query_pairs_mut().extend_pairs(
                        submission
                            .fields
                            .iter()
                            .map(|(name, value)| (name.as_str(), value.as_str())),
                    );
                    self.client.get(url)
                }
                FormRequestMethod::Post => {
                    self.client.post(&submission.url).form(&submission.fields)
                }
            };
            self.send_request(request).await
        })
    }
}

impl StdBackend {
    async fn send_bytes_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Vec<u8>, AdapterError> {
        let response = request.send().await?;
        let url = response.url().clone();
        let status = response.status();
        let status_error = response.error_for_status_ref().err();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after);
        let declared_size = response.content_length();
        let body_result = async {
            if declared_size.is_some_and(|size| size > MAX_IMAGE_RESPONSE_BYTES as u64) {
                return Err(AdapterError::ResponseTooLarge {
                    limit: MAX_IMAGE_RESPONSE_BYTES,
                });
            }
            let mut body = Vec::with_capacity(
                declared_size
                    .unwrap_or_default()
                    .min(MAX_IMAGE_RESPONSE_BYTES as u64) as usize,
            );
            let mut response = response;
            while let Some(chunk) = response.chunk().await? {
                if body.len().saturating_add(chunk.len()) > MAX_IMAGE_RESPONSE_BYTES {
                    return Err(AdapterError::ResponseTooLarge {
                        limit: MAX_IMAGE_RESPONSE_BYTES,
                    });
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        }
        .await;
        self.save_session()?;
        let rate_limit_cooldown = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let cooldown = retry_after.unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN);
            self.search_rate_limiter
                .cool_down(&url.origin().ascii_serialization(), cooldown);
            Some(cooldown)
        } else {
            None
        };
        if let Some(error) = self.cookie_jar.take_error() {
            return Err(AdapterError::CookieStore(error));
        }
        if let Some(retry_after) = rate_limit_cooldown {
            return Err(AdapterError::RateLimited { retry_after });
        }
        if let Some(error) = status_error {
            return Err(AdapterError::Http(error));
        }
        body_result
    }

    async fn send_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<PageResponse, AdapterError> {
        let response = request.send().await?;
        let url = response.url().clone();
        let status = response.status();
        let status_error = response.error_for_status_ref().err();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after);
        let body_result = response.text().await;
        self.save_session()?;
        let rate_limit_cooldown = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let cooldown = retry_after.unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN);
            self.search_rate_limiter
                .cool_down(&url.origin().ascii_serialization(), cooldown);
            Some(cooldown)
        } else {
            None
        };
        if let Some(error) = self.cookie_jar.take_error() {
            return Err(AdapterError::CookieStore(error));
        }
        if let Some(retry_after) = rate_limit_cooldown {
            return Err(AdapterError::RateLimited { retry_after });
        }
        if let Some(error) = status_error {
            return Err(AdapterError::Http(error));
        }
        Ok(PageResponse {
            url,
            body: body_result?,
        })
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    let duration = value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .and_then(|deadline| deadline.duration_since(SystemTime::now()).ok())
        })?;
    Some(duration.min(MAX_RATE_LIMIT_COOLDOWN))
}

fn derive_vault_key(
    passphrase: &str,
    salt: &[u8; VAULT_SALT_LEN],
) -> Result<Zeroizing<[u8; VAULT_KEY_LEN]>, AdapterError> {
    if passphrase.is_empty() {
        return Err(AdapterError::Vault(
            "vault passphrase must not be empty".to_owned(),
        ));
    }
    let mut key = Zeroizing::new([0; VAULT_KEY_LEN]);
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut *key)
        .map_err(|error| AdapterError::Vault(error.to_string()))?;
    Ok(key)
}

fn encrypt_vault(
    salt: &[u8; VAULT_SALT_LEN],
    key: &[u8; VAULT_KEY_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, AdapterError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|error| AdapterError::Vault(error.to_string()))?;
    let mut nonce = [0; VAULT_NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad: VAULT_MAGIC,
            },
        )
        .map_err(|_| AdapterError::Vault("unable to encrypt session data".to_owned()))?;

    let mut output =
        Vec::with_capacity(VAULT_MAGIC.len() + salt.len() + nonce.len() + ciphertext.len());
    output.extend_from_slice(VAULT_MAGIC);
    output.extend_from_slice(salt);
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

fn decrypt_vault(bytes: &[u8], passphrase: &str) -> Result<DecryptedVault, AdapterError> {
    let minimum_size = VAULT_MAGIC.len() + VAULT_SALT_LEN + VAULT_NONCE_LEN + 16;
    if bytes.len() < minimum_size || !bytes.starts_with(VAULT_MAGIC) {
        return Err(AdapterError::VaultAuthentication);
    }
    let salt_offset = VAULT_MAGIC.len();
    let nonce_offset = salt_offset + VAULT_SALT_LEN;
    let ciphertext_offset = nonce_offset + VAULT_NONCE_LEN;
    let mut salt = [0; VAULT_SALT_LEN];
    salt.copy_from_slice(&bytes[salt_offset..nonce_offset]);
    let key = derive_vault_key(passphrase, &salt)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(&*key).map_err(|_| AdapterError::VaultAuthentication)?;
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&bytes[nonce_offset..ciphertext_offset]),
            chacha20poly1305::aead::Payload {
                msg: &bytes[ciphertext_offset..],
                aad: VAULT_MAGIC,
            },
        )
        .map_err(|_| AdapterError::VaultAuthentication)?;
    Ok((salt, key, Zeroizing::new(plaintext)))
}

fn save_atomically(path: &Path, contents: &[u8]) -> Result<(), AdapterError> {
    let mut random_suffix = [0; 8];
    OsRng.fill_bytes(&mut random_suffix);
    let temporary_path = path.with_extension(format!(
        "vault-tmp-{}-{}",
        std::process::id(),
        u64::from_le_bytes(random_suffix)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary_path)?;
    let result = (|| -> Result<(), std::io::Error> {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result.map_err(AdapterError::VaultIo)
}

impl SystemAdapter for StdBackend {
    fn metrics(&self) -> Result<SystemMetrics, AdapterError> {
        let (ram_used_bytes, ram_total_bytes) = memory_metrics();
        let active_core = active_core_index();

        Ok(SystemMetrics {
            ram_used_bytes,
            ram_total_bytes,
            zpool_lz4_ratio: None,
            active_core,
        })
    }
}

#[cfg(target_os = "linux")]
fn memory_metrics() -> (Option<u64>, Option<u64>) {
    let Ok(contents) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let mut total = None;
    let mut available = None;

    for line in contents.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let Some(kibibytes) = value
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        match name {
            "MemTotal" => total = Some(kibibytes.saturating_mul(1024)),
            "MemAvailable" => available = Some(kibibytes.saturating_mul(1024)),
            _ => {}
        }
    }

    match (total, available) {
        (Some(total_bytes), Some(available_bytes)) => (
            Some(total_bytes.saturating_sub(available_bytes)),
            Some(total_bytes),
        ),
        _ => (None, None),
    }
}

#[cfg(target_os = "windows")]
fn memory_metrics() -> (Option<u64>, Option<u64>) {
    use std::mem::size_of;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return (None, None);
    }

    (
        Some(status.ullTotalPhys.saturating_sub(status.ullAvailPhys)),
        Some(status.ullTotalPhys),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn memory_metrics() -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(target_os = "linux")]
fn active_core_index() -> Option<usize> {
    let process_status = std::fs::read_to_string("/proc/self/stat").ok()?;
    let closing_parenthesis = process_status.rfind(')')?;
    process_status[closing_parenthesis + 1..]
        .split_whitespace()
        .nth(36)?
        .parse()
        .ok()
}

#[cfg(target_os = "windows")]
fn active_core_index() -> Option<usize> {
    use windows_sys::Win32::System::Threading::GetCurrentProcessorNumber;

    Some(unsafe { GetCurrentProcessorNumber() as usize })
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn active_core_index() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::{
        FormRequestMethod, FormSubmission, NetworkAdapter, ReqwestCookieStore, StdBackend,
        DESKTOP_USER_AGENT,
    };
    use reqwest::header::HeaderValue;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    #[tokio::test]
    async fn http_client_sends_user_agent_and_returns_response_body(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let bytes_read = stream.read(&mut buffer).await?;
                if bytes_read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..bytes_read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n<html>ok</html>",
                )
                .await?;
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&request).into_owned())
        });

        let backend = StdBackend::new()?;
        let body = backend
            .fetch(&format!("http://{address}/search?q=nyx"))
            .await?;
        let request = server.await??;
        let request_lowercase = request.to_ascii_lowercase();

        assert_eq!(body, "<html>ok</html>");
        assert!(request_lowercase.contains(&format!(
            "user-agent: {}",
            DESKTOP_USER_AGENT.to_ascii_lowercase()
        )));
        assert!(request_lowercase.contains("sec-ch-ua:"));
        assert!(request_lowercase.contains("accept-language: en-us,en;q=0.9"));
        assert!(request_lowercase.contains("accept: text/html,application/xhtml+xml"));
        assert!(request.contains("GET /search?q=nyx HTTP/1.1"));
        Ok(())
    }

    #[tokio::test]
    async fn fetches_binary_image_bytes_without_text_conversion(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_request(&mut stream).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\x00\xff\x80\x01")
                .await?;
            Ok::<_, std::io::Error>(())
        });

        let backend = StdBackend::new()?;
        let bytes = backend
            .fetch_bytes(&format!("http://{address}/image.png"))
            .await?;
        server.await??;

        assert_eq!(bytes, [0x00, 0xff, 0x80, 0x01]);
        Ok(())
    }

    #[tokio::test]
    async fn applies_browser_headers_to_page_fetch_and_form_post(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let page_request = read_request(&mut stream).await?;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                )
                .await?;
            let form_request = read_request(&mut stream).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await?;
            Ok::<_, std::io::Error>((page_request, form_request))
        });

        let backend = StdBackend::new()?;
        backend
            .fetch_page(&format!("http://{address}/page"))
            .await?;
        backend
            .submit_form(&FormSubmission {
                url: format!("http://{address}/login"),
                method: FormRequestMethod::Post,
                fields: vec![("q".to_owned(), "zalo".to_owned())],
            })
            .await?;

        let (page_request, form_request) = server.await??;
        for request in [&page_request, &form_request] {
            let request = String::from_utf8_lossy(request).to_ascii_lowercase();
            assert!(request.contains(&format!(
                "user-agent: {}",
                DESKTOP_USER_AGENT.to_ascii_lowercase()
            )));
            assert!(request.contains("sec-ch-ua:"));
            assert!(request.contains("sec-ch-ua-platform: \"windows\""));
            assert!(request.contains("accept-language: en-us,en;q=0.9"));
        }
        let form_request = String::from_utf8_lossy(&form_request).to_ascii_lowercase();
        assert!(form_request.starts_with("post /login http/1.1"));
        assert!(form_request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(form_request.ends_with("q=zalo"));
        Ok(())
    }

    #[tokio::test]
    async fn identifies_forbidden_search_responses() -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _request = read_request(&mut stream).await?;
            stream
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            Ok::<_, std::io::Error>(())
        });

        let backend = StdBackend::new()?;
        let error = backend
            .fetch(&format!("http://{address}/search"))
            .await
            .expect_err("403 should be returned as an HTTP error");
        assert!(error.is_forbidden());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn applies_retry_after_cooldown_after_http_429() -> Result<(), Box<dyn std::error::Error>>
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _first_request = read_request(&mut stream).await?;
            stream
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            let (mut stream, _) = listener.accept().await?;
            let _second_request = read_request(&mut stream).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await?;
            Ok::<_, std::io::Error>(())
        });

        let backend = StdBackend::new()?;
        let url = format!("http://{address}/search");
        let error = backend
            .fetch(&url)
            .await
            .expect_err("first response should be rate limited");
        assert!(error.is_rate_limited());

        let started = std::time::Instant::now();
        assert_eq!(backend.fetch(&url).await?, "ok");
        assert!(started.elapsed() >= std::time::Duration::from_millis(900));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn posts_form_with_saved_session_cookie() -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let first_request = read_request(&mut stream).await?;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=abc; Path=/; HttpOnly\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                )
                .await?;
            let second_request = read_request(&mut stream).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await?;
            Ok::<_, std::io::Error>((first_request, second_request))
        });

        let backend = StdBackend::new()?;
        let page = backend
            .fetch_page(&format!("http://{address}/login"))
            .await?;
        assert_eq!(page.body, "ok");
        let page = backend
            .submit_form(&FormSubmission {
                url: format!("http://{address}/session"),
                method: FormRequestMethod::Post,
                fields: vec![
                    ("user".to_owned(), "nyx".to_owned()),
                    ("password".to_owned(), "secret".to_owned()),
                ],
            })
            .await?;
        assert_eq!(page.body, "ok");

        let (_, form_request) = server.await??;
        let form_request = String::from_utf8_lossy(&form_request).to_ascii_lowercase();
        assert!(form_request.contains("cookie: sid=abc"));
        assert!(form_request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(form_request.ends_with("user=nyx&password=secret"));
        Ok(())
    }

    #[test]
    fn session_vault_encrypts_and_restores_cookies() -> Result<(), Box<dyn std::error::Error>> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nyx-session-vault-{}-{unique}.vault",
            std::process::id()
        ));
        let (backend, is_new) = StdBackend::open_session_vault(&path, "test passphrase")?;
        assert!(is_new);

        let url = Url::parse("https://example.test/")?;
        let cookie = HeaderValue::from_static("session=do-not-leak; Path=/; HttpOnly; Secure");
        let mut headers = [&cookie].into_iter();
        backend.cookie_jar.set_cookies(&mut headers, &url);
        backend.save_session()?;

        let encrypted = std::fs::read(&path)?;
        assert!(!String::from_utf8_lossy(&encrypted).contains("do-not-leak"));
        let (restored, is_new) = StdBackend::open_session_vault(&path, "test passphrase")?;
        assert!(!is_new);
        restored.save_session()?;
        let restored_cookie = restored
            .cookie_jar
            .cookies(&url)
            .ok_or("cookie was not restored from the vault")?;
        assert_eq!(restored_cookie.to_str()?, "session=do-not-leak");

        assert!(StdBackend::open_session_vault(&path, "wrong passphrase").is_err());
        std::fs::remove_file(path)?;
        Ok(())
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Result<Vec<u8>, std::io::Error> {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        let mut expected_len = None;
        loop {
            let bytes_read = stream.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..bytes_read]);
            if expected_len.is_none() {
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    expected_len = Some(header_end + 4 + content_len);
                }
            }
            if expected_len.is_some_and(|expected| request.len() >= expected) {
                return Ok(request);
            }
        }
        Ok(request)
    }
}
