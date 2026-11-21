use super::*;
use http::HeaderValue;
use http::header::{REFERER, USER_AGENT};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

async fn serve_once(response: &'static str) -> (Url, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/media", listener.local_addr().unwrap())).unwrap();
    let request = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), read_request(listener, response))
            .await
            .expect("client did not send a request")
    });
    (url, request)
}

async fn read_request(listener: TcpListener, response: &str) -> String {
    let (socket, _) = listener.accept().await.unwrap();
    let mut socket = BufReader::new(socket);
    let mut request = String::new();
    loop {
        let mut line = String::new();
        if socket.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
            break;
        }
        request.push_str(&line);
    }
    socket.write_all(response.as_bytes()).await.unwrap();
    request.to_ascii_lowercase()
}

fn downloader() -> Downloader {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    Downloader::new(client, 100)
}

#[tokio::test]
async fn credentials_follow_the_original_origin_when_a_preferred_mirror_returns_567() {
    let (preferred, preferred_request) =
        serve_once("HTTP/1.1 567 CDN Error\r\nContent-Length: 0\r\n\r\n").await;
    let (original, original_request) = serve_once(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4/5\r\nContent-Length: 5\r\n\r\nvideo",
    )
    .await;
    let source = DownloadSource {
        credential_origin: original.origin(),
        urls: vec![preferred, original],
    };
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_static("bytes=0-"));
    headers.insert(REFERER, HeaderValue::from_static("https://example.com/"));
    headers.insert(USER_AGENT, HeaderValue::from_static("test-agent"));
    headers.insert(COOKIE, HeaderValue::from_static("session=test"));
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer test"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video.m4s");

    let bytes = downloader()
        .to_file_with_fallback(&source, &headers, &path)
        .await
        .unwrap();
    assert_eq!(bytes, 5);
    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"video");

    let preferred_request = preferred_request.await.unwrap();
    assert!(!preferred_request.contains("\r\ncookie:"));
    assert!(!preferred_request.contains("\r\nauthorization:"));
    assert!(preferred_request.contains("\r\nrange: bytes=0-\r\n"));
    assert!(preferred_request.contains("\r\nreferer: https://example.com/\r\n"));
    assert!(preferred_request.contains("\r\nuser-agent: test-agent\r\n"));
    let original_request = original_request.await.unwrap();
    assert!(original_request.contains("\r\ncookie: session=test\r\n"));
    assert!(original_request.contains("\r\nauthorization: bearer test\r\n"));
}

#[tokio::test]
async fn rejects_an_incomplete_206_for_a_whole_file_range() {
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_static("bytes=0-"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video.m4s");
    for response in [
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/5\r\nContent-Length: 4\r\n\r\nvide",
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4/5\r\nContent-Length: 4\r\n\r\nvide",
    ] {
        let (url, request) = serve_once(response).await;
        assert!(matches!(
            downloader().to_file(&url, &headers, &path).await,
            Err(MediaError::Download(_))
        ));
        assert!(!path.exists());
        request.await.unwrap();
    }
}
