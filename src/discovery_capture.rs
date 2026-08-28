use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    time::Instant,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct CaptureOptions {
    pub listen: SocketAddr,
    pub upstream: String,
    pub max_body_bytes: usize,
    pub session_cookie: Option<String>,
    pub session_key: Vec<u8>,
}

#[derive(Debug, Serialize)]
struct BodyEvidence {
    content_type: Option<String>,
    bytes: usize,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct HttpEvidence {
    schema_version: u32,
    timestamp: chrono::DateTime<chrono::Utc>,
    request_id: String,
    session: Option<String>,
    method: String,
    path: String,
    query: BTreeMap<String, Vec<String>>,
    request: BodyEvidence,
    response: BodyEvidence,
    status: u16,
    duration_ms: f64,
}

struct Message {
    raw: Vec<u8>,
    head: String,
    body_start: usize,
    body: Vec<u8>,
}

pub async fn serve(options: CaptureOptions) -> Result<()> {
    let upstream = parse_upstream(&options.upstream)?;
    let listener = TcpListener::bind(options.listen)
        .with_context(|| format!("cannot bind discovery capture proxy to {}", options.listen))?;
    eprintln!(
        "discovery capture proxy listening on {} -> {}",
        options.listen, options.upstream
    );
    let output = Arc::new(Mutex::new(()));
    let options = Arc::new(options);
    loop {
        let (client, _) = listener.accept()?;
        let output = output.clone();
        let options = options.clone();
        let upstream = upstream.clone();
        std::thread::spawn(move || {
            if let Err(error) = handle(client, &upstream, &options, output) {
                eprintln!("discovery capture connection failed: {error:#}");
            }
        });
    }
}

fn handle(
    mut client: TcpStream,
    upstream: &str,
    options: &CaptureOptions,
    output: Arc<Mutex<()>>,
) -> Result<()> {
    let started = Instant::now();
    let timestamp = chrono::Utc::now();
    let request = read_message(&mut client, false)?;
    let request_line = request
        .head
        .lines()
        .next()
        .context("HTTP request line is missing")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .context("HTTP method is missing")?
        .to_owned();
    let target = request_parts.next().context("HTTP target is missing")?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let request_headers = parse_headers(&request.head);

    let mut server = TcpStream::connect(upstream)
        .with_context(|| format!("cannot connect to discovery capture upstream {upstream}"))?;
    server.write_all(&forward_request(&request, upstream))?;
    let response = read_message(&mut server, true)?;
    client.write_all(&response.raw)?;
    let response_line = response
        .head
        .lines()
        .next()
        .context("HTTP status line is missing")?;
    let status = response_line
        .split_whitespace()
        .nth(1)
        .context("HTTP status is missing")?
        .parse()?;
    let response_headers = parse_headers(&response.head);

    let evidence = HttpEvidence {
        schema_version: 1,
        timestamp,
        request_id: Uuid::now_v7().to_string(),
        session: session_hash(&request_headers, options),
        method,
        path: path.to_owned(),
        query: parse_query(query),
        request: body_evidence(
            &request.body,
            request_headers.get("content-type").cloned(),
            options.max_body_bytes,
        ),
        response: body_evidence(
            &response.body,
            response_headers.get("content-type").cloned(),
            options.max_body_bytes,
        ),
        status,
        duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
    };
    let mut line = serde_json::to_vec(&evidence)?;
    line.push(b'\n');
    let _guard = output.lock().expect("capture output mutex is poisoned");
    let mut writer = std::io::stdout().lock();
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

fn read_message(stream: &mut TcpStream, response: bool) -> Result<Message> {
    let mut raw = Vec::new();
    let header_end = loop {
        if raw.len() > 1024 * 1024 {
            bail!("HTTP header exceeds 1 MiB");
        }
        let mut chunk = [0_u8; 8192];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            bail!("connection closed before HTTP header completed");
        }
        raw.extend_from_slice(&chunk[..read]);
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let headers = parse_headers(&head);
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok());
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        read_until_chunked_end(stream, &mut raw, header_end)?;
    } else if let Some(length) = content_length {
        while raw.len() < header_end + length {
            let mut chunk = [0_u8; 8192];
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..read]);
        }
    } else if response {
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest)?;
        raw.extend(rest);
    }
    let encoded_body = &raw[header_end..];
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        decode_chunked(encoded_body).unwrap_or_else(|| encoded_body.to_vec())
    } else {
        encoded_body.to_vec()
    };
    Ok(Message {
        raw,
        head,
        body_start: header_end,
        body,
    })
}

fn read_until_chunked_end(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    body_start: usize,
) -> Result<()> {
    loop {
        if chunked_complete(&raw[body_start..]) {
            return Ok(());
        }
        let mut chunk = [0_u8; 8192];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            bail!("connection closed before chunked body completed");
        }
        raw.extend_from_slice(&chunk[..read]);
    }
}

fn chunked_complete(mut body: &[u8]) -> bool {
    loop {
        let Some(end) = body.windows(2).position(|window| window == b"\r\n") else {
            return false;
        };
        let Ok(size) = usize::from_str_radix(
            String::from_utf8_lossy(&body[..end])
                .split(';')
                .next()
                .unwrap_or("")
                .trim(),
            16,
        ) else {
            return false;
        };
        body = &body[end + 2..];
        if size == 0 {
            return body.windows(4).any(|window| window == b"\r\n\r\n")
                || body.starts_with(b"\r\n");
        }
        if body.len() < size + 2 {
            return false;
        }
        body = &body[size + 2..];
    }
}

fn decode_chunked(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    loop {
        let end = body.windows(2).position(|window| window == b"\r\n")?;
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&body[..end])
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        body = &body[end + 2..];
        if size == 0 {
            return Some(decoded);
        }
        if body.len() < size + 2 {
            return None;
        }
        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn body_evidence(bytes: &[u8], content_type: Option<String>, limit: usize) -> BodyEvidence {
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    if bytes.len() > limit {
        return BodyEvidence {
            content_type,
            bytes: bytes.len(),
            sha256,
            body: None,
            omitted: Some("too_large"),
            parse_error: None,
        };
    }
    if !capturable(content_type.as_deref()) {
        return BodyEvidence {
            content_type,
            bytes: bytes.len(),
            sha256,
            body: None,
            omitted: Some("unsupported_content_type"),
            parse_error: None,
        };
    }
    if bytes.is_empty() {
        return BodyEvidence {
            content_type,
            bytes: 0,
            sha256,
            body: Some(Value::Null),
            omitted: None,
            parse_error: None,
        };
    }
    let is_json = content_type
        .as_deref()
        .is_some_and(|value| value.contains("json"));
    if is_json {
        match serde_json::from_slice(bytes) {
            Ok(body) => BodyEvidence {
                content_type,
                bytes: bytes.len(),
                sha256,
                body: Some(body),
                omitted: None,
                parse_error: None,
            },
            Err(error) => BodyEvidence {
                content_type,
                bytes: bytes.len(),
                sha256,
                body: Some(Value::String(String::from_utf8_lossy(bytes).into_owned())),
                omitted: None,
                parse_error: Some(error.to_string()),
            },
        }
    } else if content_type
        .as_deref()
        .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
    {
        let body = parse_pairs(&String::from_utf8_lossy(bytes));
        BodyEvidence {
            content_type,
            bytes: bytes.len(),
            sha256,
            body: Some(serde_json::to_value(body).expect("form map is serializable")),
            omitted: None,
            parse_error: None,
        }
    } else {
        BodyEvidence {
            content_type,
            bytes: bytes.len(),
            sha256,
            body: Some(Value::String(String::from_utf8_lossy(bytes).into_owned())),
            omitted: None,
            parse_error: None,
        }
    }
}

fn capturable(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|value| {
        value.starts_with("text/")
            || value.contains("json")
            || value.starts_with("application/x-www-form-urlencoded")
    })
}

fn parse_headers(head: &str) -> BTreeMap<String, String> {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn forward_request(request: &Message, upstream: &str) -> Vec<u8> {
    let mut lines = request.head.trim_end_matches("\r\n").lines();
    let mut forwarded = Vec::new();
    if let Some(request_line) = lines.next() {
        forwarded.extend_from_slice(request_line.as_bytes());
        forwarded.extend_from_slice(b"\r\n");
    }
    for line in lines {
        let name = line
            .split_once(':')
            .map(|(name, _)| name.trim().to_ascii_lowercase());
        if matches!(
            name.as_deref(),
            Some("host" | "connection" | "accept-encoding")
        ) {
            continue;
        }
        forwarded.extend_from_slice(line.as_bytes());
        forwarded.extend_from_slice(b"\r\n");
    }
    forwarded
        .extend_from_slice(format!("Host: {upstream}\r\nConnection: close\r\n\r\n").as_bytes());
    forwarded.extend_from_slice(&request.raw[request.body_start..]);
    forwarded
}

fn session_hash(headers: &BTreeMap<String, String>, options: &CaptureOptions) -> Option<String> {
    let name = options.session_cookie.as_ref()?;
    let cookie = headers.get("cookie")?;
    let value = cookie
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))?;
    Some(format!(
        "hmac-sha256:{}",
        hmac_sha256_hex(&options.session_key, value.as_bytes())
    ))
}

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut key_block = [0_u8; 64];
    if key.len() > 64 {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for index in 0..64 {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    format!("{:x}", outer.finalize())
}

fn parse_query(query: &str) -> BTreeMap<String, Vec<String>> {
    parse_pairs(query)
}

fn parse_pairs(encoded: &str) -> BTreeMap<String, Vec<String>> {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for pair in encoded.split('&').filter(|value| !value.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        values
            .entry(percent_decode(key))
            .or_default()
            .push(percent_decode(value));
    }
    values
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    decoded.push(high * 16 + low);
                    index += 2;
                } else {
                    decoded.push(bytes[index]);
                }
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_upstream(upstream: &str) -> Result<String> {
    let value = upstream
        .strip_prefix("http://")
        .context("capture upstream must use http://")?
        .trim_end_matches('/');
    if value.is_empty() || value.contains('/') {
        bail!("capture upstream must contain only an http:// host and optional port");
    }
    Ok(if value.contains(':') {
        value.to_owned()
    } else {
        format!("{value}:80")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_json_and_omits_large_or_binary_bodies() {
        let json = body_evidence(br#"{"id":7}"#, Some("application/json".into()), 100);
        assert_eq!(json.body.unwrap()["id"], 7);
        assert_eq!(
            body_evidence(b"long", Some("text/plain".into()), 2).omitted,
            Some("too_large")
        );
        assert_eq!(
            body_evidence(b"jpeg", Some("image/jpeg".into()), 100).omitted,
            Some("unsupported_content_type")
        );
    }

    #[test]
    fn hashes_configured_session_cookie() {
        let headers = BTreeMap::from([("cookie".into(), "foo=x; session=secret".into())]);
        let options = CaptureOptions {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream: "http://127.0.0.1:1".into(),
            max_body_bytes: 10,
            session_cookie: Some("session".into()),
            session_key: b"run-key".to_vec(),
        };
        let value = session_hash(&headers, &options).unwrap();
        assert!(value.starts_with("hmac-sha256:"));
        assert!(!value.contains("secret"));
    }

    #[test]
    fn decodes_chunked_body() {
        assert_eq!(decode_chunked(b"4\r\ntest\r\n0\r\n\r\n").unwrap(), b"test");
        assert!(chunked_complete(b"4\r\ntest\r\n0\r\n\r\n"));
    }

    #[test]
    fn parses_query_and_form_values() {
        let values = parse_pairs("name=hello+world&tag=a&tag=b%20c");
        assert_eq!(values["name"], ["hello world"]);
        assert_eq!(values["tag"], ["a", "b c"]);
        let form = body_evidence(
            b"amount=10&name=alice",
            Some("application/x-www-form-urlencoded".into()),
            100,
        );
        assert_eq!(form.body.unwrap()["amount"][0], "10");
    }
}
