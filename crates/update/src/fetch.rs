//! The release check and downloads. HTTPS only, redirects included (GitHub sends assets to its object storage);
//! no cookies, no cache, no credentials; the only headers are Accept and a User-Agent, which GitHub requires.

use crate::source::scheme_allowed;
use crate::{Result, UpdateError};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// Largest release list answer.
pub const MAX_LIST: u64 = 1_048_576;
/// Largest SHA256SUMS or signature file.
pub const MAX_SMALL: u64 = 65_536;
/// Largest release zip.
pub const MAX_ZIP: u64 = 4 << 30;

/// How the updater reads URLs; tests substitute failing or truncating fetchers.
pub trait Fetch {
    /// The whole body, refused past `max` bytes.
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>>;
    /// Streams the body into `dest`, refused past `max` bytes. A short body is an error.
    fn download(&self, url: &str, dest: &Path, max: u64) -> Result<()>;
}

pub struct HttpFetch {
    client: reqwest::blocking::Client,
}

impl HttpFetch {
    pub fn new() -> Result<HttpFetch> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 10 {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() == "https" || (cfg!(test) && scheme_allowed(attempt.url().as_str())) {
                attempt.follow()
            } else {
                attempt.error("redirect away from HTTPS refused")
            }
        });
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("Heimdall-Updater/", env!("CARGO_PKG_VERSION")))
            .redirect(policy)
            .https_only(!cfg!(test))
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .map_err(|e| UpdateError::new(format!("could not start the update client: {e}")))?;
        Ok(HttpFetch { client })
    }

    fn open(&self, url: &str) -> Result<Box<dyn Read>> {
        if !scheme_allowed(url) {
            return Err(UpdateError::new(format!("refused a non-HTTPS address: {url}")));
        }
        if let Some(path) = url.strip_prefix("file://") {
            let file = std::fs::File::open(path).map_err(|_| UpdateError::new(format!("no file at {path}")))?;
            return Ok(Box::new(file));
        }
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json, application/octet-stream")
            .send()
            .map_err(|e| UpdateError::new(format!("could not reach the release server: {e}")))?;
        match response.status().as_u16() {
            200 => Ok(Box::new(Checked { expected: response.content_length(), read: 0, inner: response })),
            404 => Err(UpdateError::new("no published release found")),
            403 | 429 => Err(UpdateError::new("GitHub refused the update check (rate limit); try again later")),
            code => Err(UpdateError::new(format!("the release server answered HTTP {code}"))),
        }
    }
}

/// A body that must be as long as its Content-Length said.
struct Checked<R> {
    expected: Option<u64>,
    read: u64,
    inner: R,
}

impl<R: Read> Read for Checked<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if n == 0 && !buf.is_empty() && self.expected.is_some_and(|e| e != self.read) {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the download ended early"));
        }
        Ok(n)
    }
}

impl Fetch for HttpFetch {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>> {
        let mut body = vec![];
        self.open(url)?
            .take(max + 1)
            .read_to_end(&mut body)
            .map_err(|e| UpdateError::new(format!("download of {} failed: {e}", name(url))))?;
        if body.len() as u64 > max {
            return Err(UpdateError::new(format!("{} is too large", name(url))));
        }
        Ok(body)
    }

    fn download(&self, url: &str, dest: &Path, max: u64) -> Result<()> {
        let mut reader = self.open(url)?.take(max + 1);
        let mut file = std::fs::File::create(dest)?;
        let copied = std::io::copy(&mut reader, &mut file)
            .map_err(|e| UpdateError::new(format!("download of {} failed: {e}", name(url))))?;
        if copied > max {
            return Err(UpdateError::new(format!("{} is too large", name(url))));
        }
        file.flush()?;
        Ok(())
    }
}

fn name(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}
