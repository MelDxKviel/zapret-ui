//! Bounded streaming downloads shared by core installation and app updates.
//! Callers choose the request/TLS policy and own temporary-file cleanup, checksum
//! verification and promotion. No response body or headers are written to logs.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// Download into a caller-owned temporary path, returning its SHA-256. A failed
/// download may leave a partial file, but never writes past `max_bytes`. HTTP
/// errors are rejected before creating/truncating the destination. Progress
/// reports completed writes; an absent Content-Length remains `None`.
pub(crate) async fn to_file(
    request: reqwest::RequestBuilder,
    destination: &Path,
    max_bytes: u64,
    on_progress: impl Fn(u64, Option<u64>) + Send + Sync,
) -> Result<String> {
    let mut response = request
        .send()
        .await
        .context("Failed to send download request")?;
    if !response.status().is_success() {
        bail!("Failed to download asset: HTTP {}", response.status());
    }
    let total = response.content_length();
    let mut file = tokio::fs::File::create(destination)
        .await
        .context("Failed to create temporary download file")?;
    let mut downloaded = 0u64;
    let mut hasher = Sha256::new();
    on_progress(0, total);
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Error reading download stream")?
    {
        if chunk.len() as u64 > max_bytes.saturating_sub(downloaded) {
            bail!("Download aborted: asset exceeds the {max_bytes} byte safety limit");
        }
        file.write_all(&chunk)
            .await
            .context("Failed to write download chunk")?;
        downloaded += chunk.len() as u64;
        hasher.update(&chunk);
        on_progress(downloaded, total);
    }
    file.flush().await.context("Failed to flush download")?;
    drop(file);
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Mutex, time::Duration};
    use tokio::{io::AsyncReadExt, net::TcpSocket};

    /// One loopback request with an explicit raw HTTP response. Chunked bodies
    /// exercise the byte limit without a trusted Content-Length or real network.
    async fn response_server(
        response: &'static [u8],
    ) -> (reqwest::RequestBuilder, tokio::task::JoinHandle<()>) {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let url = format!("http://{}/artifact", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let len = stream.read(&mut buffer).await.unwrap();
                    assert!(len > 0, "request ended before its headers");
                    request.extend_from_slice(&buffer[..len]);
                    assert!(request.len() < 16 * 1024);
                }
                assert!(request.starts_with(b"GET /artifact HTTP/1.1\r\n"));
                stream.write_all(response).await.unwrap();
            })
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        (client.get(url), task)
    }

    #[tokio::test]
    async fn download_hashes_written_bytes_and_reports_known_or_unknown_total() {
        for (response, total) in [
            (b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".as_slice(), Some(3)),
            (b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n1\r\na\r\n2\r\nbc\r\n0\r\n\r\n".as_slice(), None),
        ] {
            let (request, server) = response_server(response).await;
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("download");
            let progress = Mutex::new(Vec::new());
            let hash = to_file(request, &path, 3, |bytes, total| {
                progress.lock().unwrap().push((bytes, total));
            }).await.unwrap();
            server.await.unwrap();
            assert_eq!(std::fs::read(path).unwrap(), b"abc");
            assert_eq!(hash, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
            let progress = progress.into_inner().unwrap();
            assert_eq!(progress.first(), Some(&(0, total)));
            assert_eq!(progress.last(), Some(&(3, total)));
            assert!(progress.iter().all(|(_, reported)| *reported == total));
            assert!(progress.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        }
    }

    #[tokio::test]
    async fn http_error_does_not_truncate_destination_or_expose_body() {
        let (request, server) = response_server(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 12\r\nConnection: close\r\n\r\nprivate-body").await;
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("download");
        std::fs::write(&path, "previous").unwrap();
        let error = to_file(request, &path, 100, |_, _| {
            panic!("failed HTTP response must not report download progress")
        })
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(error.to_string().contains("503"));
        assert!(!format!("{error:#}").contains("private-body"));
        assert_eq!(std::fs::read(path).unwrap(), b"previous");
    }

    #[tokio::test]
    async fn chunked_oversize_never_writes_or_reports_more_than_the_limit() {
        let (request, server) = response_server(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n").await;
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("download");
        let progress = Mutex::new(Vec::new());
        let error = to_file(request, &path, 4, |bytes, total| {
            progress.lock().unwrap().push((bytes, total));
        })
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(error.to_string().contains("safety limit"));
        assert!(std::fs::metadata(path).unwrap().len() <= 4);
        assert!(progress
            .lock()
            .unwrap()
            .iter()
            .all(|(bytes, total)| *bytes <= 4 && total.is_none()));
    }

    #[tokio::test]
    async fn incomplete_body_is_an_error_instead_of_a_successful_hash() {
        let (request, server) = response_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc",
        )
        .await;
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("download");
        assert!(to_file(request, &path, 6, |_, _| {}).await.is_err());
        server.await.unwrap();
        assert!(std::fs::metadata(path).unwrap().len() <= 3);
    }
}
