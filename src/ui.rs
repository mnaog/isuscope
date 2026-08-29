use crate::{config::LoadedConfig, diff, report, shutdown::Shutdown, storage::Store};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
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
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query_params(query);
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
        "/diff" => {
            let body = match (query.get("base"), query.get("candidate")) {
                (Some(base), Some(candidate)) => {
                    let diff = match load_diff(config, base, candidate) {
                        Ok(diff) => diff,
                        Err(error) => {
                            let body = format!("isuscope diff is unavailable: {error:#}\n");
                            return response(
                                &mut stream,
                                500,
                                "text/plain; charset=utf-8",
                                body.as_bytes(),
                            );
                        }
                    };
                    let mut body = Vec::new();
                    diff::write_html(&diff, &mut body)?;
                    body
                }
                _ => diff_form(config)?.into_bytes(),
            };
            response(&mut stream, 200, "text/html; charset=utf-8", &body)
        }
        "/api/diff" => {
            let (Some(base), Some(candidate)) = (query.get("base"), query.get("candidate")) else {
                return response(
                    &mut stream,
                    400,
                    "application/json; charset=utf-8",
                    br#"{"error":"base and candidate query parameters are required"}"#,
                );
            };
            let diff = match load_diff(config, base, candidate) {
                Ok(diff) => diff,
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
            diff::write_json(&diff, &mut body)?;
            response(&mut stream, 200, "application/json; charset=utf-8", &body)
        }
        "/favicon.ico" => response(&mut stream, 204, "image/x-icon", b""),
        _ => response(&mut stream, 404, "text/plain; charset=utf-8", b"not found"),
    }
}

fn load_diff(config: &LoadedConfig, base: &str, candidate: &str) -> Result<diff::RunDiff> {
    let store = Store::open(&config.data_dir)?;
    let base = store
        .resolve_id(base)?
        .with_context(|| format!("base run `{base}` was not found"))?;
    let candidate = store
        .resolve_id(candidate)?
        .with_context(|| format!("candidate run `{candidate}` was not found"))?;
    let base = report::diagnose(
        store.load(&base)?,
        store.metrics(&base)?,
        store.transitions(&base)?,
        store.final_dir(&base).join("logs"),
        None,
    );
    let candidate = report::diagnose(
        store.load(&candidate)?,
        store.metrics(&candidate)?,
        store.transitions(&candidate)?,
        store.final_dir(&candidate).join("logs"),
        None,
    );
    Ok(diff::build(base, candidate))
}

fn diff_form(config: &LoadedConfig) -> Result<String> {
    let store = Store::open(&config.data_dir)?;
    let runs = store.list(100)?;
    let options = |selected: usize| {
        runs.iter()
            .enumerate()
            .map(|(index, run)| {
                let label = format!(
                    "{} · score {} · {}",
                    short(&run.id),
                    run.score
                        .map_or_else(|| "-".into(), |score| score.to_string()),
                    if run.tags.is_empty() {
                        run.started_at.clone()
                    } else {
                        run.tags.join(",")
                    }
                );
                format!(
                    "<option value=\"{}\"{}>{}</option>",
                    escape(&run.id),
                    if index == selected { " selected" } else { "" },
                    escape(&label)
                )
            })
            .collect::<String>()
    };
    let base_options = options(if runs.len() > 1 { 1 } else { 0 });
    let candidate_options = options(0);
    Ok(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>isuscope diff</title><style>:root{{color-scheme:dark;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#0b0f14;color:#d8dee9}}main{{max-width:760px;margin:60px auto;padding:24px}}form{{display:grid;gap:16px;background:#121923;border:1px solid #263241;border-radius:10px;padding:20px}}select,button{{font:inherit;padding:10px;background:#0b0f14;color:#d8dee9;border:1px solid #46566b;border-radius:6px}}a{{color:#88c0d0}}</style></head><body><main><p><a href="/">latest report</a></p><h1>Compare runs</h1><form action="/diff" method="get"><label>Base<select name="base" required>{base_options}</select></label><label>Candidate<select name="candidate" required>{candidate_options}</select></label><button type="submit">Compare</button></form></main></body></html>"#
    ))
}

fn query_params(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Some((percent_decode(key)?, percent_decode(value)?))
        })
        .collect()
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
                output.push(u8::from_str_radix(hex, 16).ok()?);
                index += 2;
            }
            b'%' => return None,
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8(output).ok()
}

fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(8)..).unwrap_or(id)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'\r\nConnection: close\r\n\r\n",
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
