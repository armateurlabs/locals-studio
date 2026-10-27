use reqwest::{redirect::Policy, Client};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use url::{Host, Url};

const RESPONSE_LIMIT: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn models_url(endpoint: &str) -> Result<Url, String> {
    let endpoint = endpoint.trim();
    let mut url =
        Url::parse(endpoint).map_err(|_| "The API endpoint is not a valid URL.".to_owned())?;
    let authority = endpoint
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    let hostname = match url.host() {
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => true,
        Some(Host::Domain(host)) => {
            !host.is_empty()
                && host.len() <= 253
                && host.trim_end_matches('.').split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
        }
        None => false,
    };
    if url.scheme() != "http"
        || authority.is_empty()
        || !hostname
        || !url.username().is_empty()
        || url.password().is_some()
        || authority.contains('@')
        || endpoint.chars().any(char::is_whitespace)
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "/v1" | "/v1/")
    {
        return Err("Use an HTTP API endpoint ending in /v1, with an IP address or hostname and no credentials, query or fragment.".into());
    }
    url.set_path("/v1/models");
    Ok(url)
}

fn transport_error(error: reqwest::Error, context: &str) -> String {
    if error.is_timeout() {
        "The API request timed out.".into()
    } else {
        format!("{context}: {error}")
    }
}

async fn fetch_models(endpoint: &str, timeout: Duration) -> Result<Value, String> {
    let url = models_url(endpoint)?;
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|error| format!("Could not create the API test client: {error}"))?;
    let started = Instant::now();
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| transport_error(error, "Could not reach the API"))?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|size| size > RESPONSE_LIMIT as u64)
    {
        return Err("The API response exceeds the 1 MiB test limit.".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| transport_error(error, "Could not read the API response"))?
    {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return Err("The API response exceeds the 1 MiB test limit.".into());
        }
        body.extend_from_slice(&chunk);
    }
    // HTTP errors are results too: showing their original body helps explain the failure.
    Ok(json!({
        "url": url.as_str(),
        "status": status.as_u16(),
        "status_text": status.canonical_reason().unwrap_or(""),
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "body": String::from_utf8_lossy(&body),
    }))
}

#[tauri::command]
pub async fn test_api(endpoint: String) -> Result<Value, String> {
    fetch_models(&endpoint, REQUEST_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_response(response: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                if socket.read(&mut byte).await.unwrap() == 0 || request.len() >= 8192 {
                    break;
                }
                request.push(byte[0]);
            }
            // Oversized responses may be deliberately rejected before the socket finishes writing.
            let _ = socket.write_all(&response).await;
            String::from_utf8(request).unwrap()
        });
        (endpoint, task)
    }

    #[test]
    fn only_http_v1_endpoints_without_credentials_or_url_extras_are_accepted() {
        for (endpoint, expected) in [
            (
                "http://127.0.0.1:8321/v1",
                "http://127.0.0.1:8321/v1/models",
            ),
            ("http://[::1]:8321/v1/", "http://[::1]:8321/v1/models"),
            (
                "http://localhost:8321/v1",
                "http://localhost:8321/v1/models",
            ),
            (
                "http://mac-studio.local:8321/v1",
                "http://mac-studio.local:8321/v1/models",
            ),
            (
                "http://192.168.1.9:8321/v1/",
                "http://192.168.1.9:8321/v1/models",
            ),
        ] {
            assert_eq!(models_url(endpoint).unwrap().as_str(), expected);
        }
        for endpoint in [
            "https://localhost/v1",
            "file:///v1",
            "http://localhost/",
            "http://localhost/v1/models",
            "http://localhost/v1/other",
            "http://user@localhost/v1",
            "http://user:secret@localhost/v1",
            "http://@localhost/v1",
            "http://localhost/v1?",
            "http://localhost/v1#",
            "http://local\nhost/v1",
            "http://-invalid.local/v1",
            "http:localhost/v1",
            "not a URL",
        ] {
            assert!(models_url(endpoint).is_err(), "{endpoint}");
        }
    }

    #[tokio::test]
    async fn successful_model_json_is_returned_without_changing_the_body() {
        let body = "{\"data\":[{\"id\":\"example-thinking\"}]}\n";
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (endpoint, request) = mock_response(response.into_bytes()).await;
        let result = test_api(endpoint.clone()).await.unwrap();
        assert_eq!(
            result["url"],
            endpoint.trim_end_matches('/').to_owned() + "/models"
        );
        assert_eq!(result["status"], 200);
        assert_eq!(result["status_text"], "OK");
        assert_eq!(result["body"], body);
        assert!(result["elapsed_ms"].is_u64());
        assert!(request
            .await
            .unwrap()
            .starts_with("GET /v1/models HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn http_errors_and_non_json_responses_remain_inspectable() {
        for (status, body) in [
            ("200 OK", "This endpoint is not JSON"),
            ("401 Unauthorized", "{\"error\":\"API key required\"}"),
            ("503 Service Unavailable", "Worker is loading"),
        ] {
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let (endpoint, request) = mock_response(response.into_bytes()).await;
            let result = test_api(endpoint).await.unwrap();
            assert_eq!(result["status"], status[..3].parse::<u16>().unwrap());
            assert_eq!(result["body"], body);
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn redirects_are_returned_instead_of_followed() {
        let (endpoint, request) = mock_response(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 5\r\nConnection: close\r\n\r\nmoved".to_vec()).await;
        let result = test_api(endpoint).await.unwrap();
        assert_eq!(result["status"], 302);
        assert_eq!(result["body"], "moved");
        request.await.unwrap();
    }

    #[tokio::test]
    async fn both_declared_and_streamed_oversized_responses_are_bounded() {
        for declared in [true, false] {
            let response = if declared {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    RESPONSE_LIMIT + 1
                )
                .into_bytes()
            } else {
                let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
                response.extend(vec![b'x'; RESPONSE_LIMIT + 1]);
                response
            };
            let (endpoint, request) = mock_response(response).await;
            assert!(test_api(endpoint).await.unwrap_err().contains("1 MiB"));
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_timeout_also_bounds_waiting_for_the_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let error = fetch_models(&endpoint, Duration::from_millis(50))
            .await
            .unwrap_err();
        task.abort();
        assert!(error.contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn connection_failures_have_a_readable_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        drop(listener);
        assert!(test_api(endpoint)
            .await
            .unwrap_err()
            .contains("Could not reach the API"));
    }
}
