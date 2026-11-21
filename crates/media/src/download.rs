use feedbot_core::MediaError;
use feedbot_core::extract::DownloadSource;
use http::header::{AUTHORIZATION, CONTENT_RANGE, COOKIE, RANGE};
use http::{HeaderMap, StatusCode};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use url::Url;

/// Streams HTTP bodies to disk with a hard size cap, so one large upstream
/// file cannot exhaust memory or disk.
#[derive(Clone)]
pub struct Downloader {
    client: reqwest::Client,
    max_bytes: u64,
}

impl Downloader {
    pub fn new(client: reqwest::Client, max_bytes: u64) -> Self {
        Self {
            client,
            max_bytes: max_bytes.max(1),
        }
    }

    /// Client suited to large bodies: no overall deadline, only connect and
    /// per-read timeouts, so slow but progressing downloads are not cut off.
    pub fn build(
        user_agent: &str,
        connect_timeout: Duration,
        read_timeout: Duration,
        max_bytes: u64,
    ) -> Result<Self, MediaError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| MediaError::Download(e.to_string()))?;
        Ok(Self::new(client, max_bytes))
    }

    pub async fn to_file(
        &self,
        url: &Url,
        headers: &HeaderMap,
        path: &Path,
    ) -> Result<u64, MediaError> {
        let mut builder = self.client.get(url.clone());
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let mut resp = builder
            .send()
            .await
            .map_err(|e| MediaError::Download(e.without_url().to_string()))?;
        if !resp.status().is_success() {
            return Err(MediaError::Download(format!("HTTP {}", resp.status())));
        }
        let expected_bytes =
            if resp.status() == StatusCode::PARTIAL_CONTENT
                && headers
                    .get(RANGE)
                    .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"bytes=0-"))
            {
                Some(complete_range_length(resp.headers()).ok_or_else(|| {
                    MediaError::Download("HTTP 206: 媒体响应未覆盖完整文件".into())
                })?)
            } else {
                None
            };
        if [resp.content_length(), expected_bytes]
            .into_iter()
            .flatten()
            .any(|n| n > self.max_bytes)
        {
            return Err(self.too_large());
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;
        let result = self.write_body(&mut resp, &mut file, expected_bytes).await;
        if result.is_err() {
            let _ = file.flush().await;
            drop(file);
            let _ = tokio::fs::remove_file(path).await;
        }
        result
    }

    /// Try mirrors of one file in the order provided. Send account
    /// credentials only to the source's credential origin.
    pub async fn to_file_with_fallback(
        &self,
        source: &DownloadSource,
        headers: &HeaderMap,
        path: &Path,
    ) -> Result<u64, MediaError> {
        let mut public_headers = headers.clone();
        public_headers.remove(COOKIE);
        public_headers.remove(AUTHORIZATION);
        let mut last_error = None;
        for url in &source.urls {
            let request_headers = if url.origin() == source.credential_origin {
                headers
            } else {
                &public_headers
            };
            match self.to_file(url, request_headers, path).await {
                Ok(bytes) => return Ok(bytes),
                Err(error @ MediaError::Download(_)) => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| MediaError::Download("没有可用的下载地址".into())))
    }

    async fn write_body(
        &self,
        response: &mut reqwest::Response,
        file: &mut tokio::fs::File,
        expected_bytes: Option<u64>,
    ) -> Result<u64, MediaError> {
        let mut written = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| MediaError::Download(e.without_url().to_string()))?
        {
            written += chunk.len() as u64;
            if written > self.max_bytes {
                return Err(self.too_large());
            }
            if expected_bytes.is_some_and(|n| written > n) {
                return Err(MediaError::Download(
                    "HTTP 206: 响应长度与 Content-Range 不符".into(),
                ));
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| MediaError::Io(e.to_string()))?;
        }
        if expected_bytes.is_some_and(|n| written != n) {
            return Err(MediaError::Download(
                "HTTP 206: 响应长度与 Content-Range 不符".into(),
            ));
        }
        file.flush()
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;
        Ok(written)
    }

    fn too_large(&self) -> MediaError {
        MediaError::TooLarge(self.max_bytes)
    }
}

fn complete_range_length(headers: &HeaderMap) -> Option<u64> {
    let (unit, range) = headers.get(CONTENT_RANGE)?.to_str().ok()?.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (bounds, size) = range.split_once('/')?;
    let (start, end) = bounds.split_once('-')?;
    let parse_number = |value: &str| {
        if value.bytes().all(|byte| byte.is_ascii_digit()) {
            value.parse::<u64>().ok()
        } else {
            None
        }
    };
    let size = parse_number(size)?;
    (parse_number(start)? == 0 && parse_number(end)?.checked_add(1) == Some(size)).then_some(size)
}

#[cfg(test)]
mod tests;
