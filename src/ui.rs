use crate::{config::LoadedConfig, report, shutdown::Shutdown, storage::Store};
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

pub const ADDRESS: &str = "127.0.0.1:3000";

pub async fn serve(config: LoadedConfig, mut shutdown: Shutdown) -> Result<()> {
    let listener =
        TcpListener::bind(ADDRESS).with_context(|| format!("cannot bind UI to {ADDRESS}"))?;
    listener.set_nonblocking(true)?;
    println!("isuscope UI  http://{ADDRESS}");
    println!("press Ctrl-C to stop");
    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(error) = respond(stream, &config) {
                            eprintln!("! UI request failed: {error:#}");
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                }
            }
            _ = shutdown.cancelled() => return Ok(()),
        }
    }
}

fn respond(mut stream: TcpStream, config: &LoadedConfig) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut request = vec![0; 8192];
    let length = stream.read(&mut request)?;
    let request = String::from_utf8_lossy(&request[..length]);
    let Some(line) = request.lines().next() else {
        return response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"bad request",
        );
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    if method != "GET" {
        return response(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            b"method not allowed",
        );
    }
    match path {
        "/" | "/index.html" => {
            let report = match latest_report(config) {
                Ok(report) => report,
                Err(error) => {
                    let body = format!("isuscope UI is unavailable: {error:#}\n");
                    return response(
                        &mut stream,
                        500,
                        "text/plain; charset=utf-8",
                        body.as_bytes(),
                    );
                }
            };
            let mut body = Vec::new();
            report::write_html(&report, &mut body)?;
            response(&mut stream, 200, "text/html; charset=utf-8", &body)
        }
        "/api/report" => {
            let report = match latest_report(config) {
                Ok(report) => report,
                Err(error) => {
                    let body = serde_json::json!({"error": format!("{error:#}")}).to_string();
                    return response(
                        &mut stream,
                        500,
                        "application/json; charset=utf-8",
                        body.as_bytes(),
                    );
                }
            };
            let mut body = Vec::new();
            report::write_json(&report, &mut body)?;
            response(&mut stream, 200, "application/json; charset=utf-8", &body)
        }
        "/favicon.ico" => response(&mut stream, 204, "image/x-icon", b""),
        _ => response(&mut stream, 404, "text/plain; charset=utf-8", b"not found"),
    }
}

fn latest_report(config: &LoadedConfig) -> Result<report::RunReport> {
    let store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id("latest")?
        .context("no finalized runs; execute `isuscope run` first")?;
    Ok(report::build(
        store.load(&id)?,
        store.metrics(&id)?,
        store.transitions(&id)?,
        store.final_dir(&id).join("logs"),
        Some(config.data_dir.join("latest/logs")),
        false,
    ))
}

fn response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_is_fixed_to_loopback_without_configuration() {
        assert_eq!(ADDRESS, "127.0.0.1:3000");
    }
}
