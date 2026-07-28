use crate::config::Config;
use crate::mail::{normalize_from_address, parse_single_addr, replace_addresses};
use crate::simplelogin_client::SimpleLoginClient;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_native_tls::TlsConnector;

pub struct RelayHandler {
    config: Config,
    sl: Arc<SimpleLoginClient>,
}

impl RelayHandler {
    pub fn new(config: Config, sl: Arc<SimpleLoginClient>) -> Self {
        Self { config, sl }
    }

    /// Public DATA behavior: the whole processing operation is wrapped in the
    /// configured DATA_TIMEOUT, precisely as asyncio.wait_for(_process()).
    pub async fn process(&self, mail_from: String, rcpt_tos: Vec<String>, data: Vec<u8>) -> String {
        let config = self.config.clone();
        let sl = Arc::clone(&self.sl);
        match tokio::time::timeout(Duration::from_secs(config.data_timeout), async move {
            tokio::task::spawn_blocking(move || {
                process_sync(config, sl, &mail_from, &rcpt_tos, &data)
            })
            .await
            .map_err(|e| anyhow!(e))?
        })
        .await
        {
            Ok(Ok(())) => "250 OK".to_string(),
            Ok(Err(e)) => {
                if let Some(diagnostic) = e
                    .chain()
                    .map(ToString::to_string)
                    .find(|message| message.starts_with("Upstream SMTP error"))
                {
                    log::error!("{diagnostic}");
                    return format!("451 {diagnostic}");
                }
                log::error!("Unexpected error: {e:#}");
                "451 Internal error".to_string()
            }
            Err(_) => {
                log::error!("handle_DATA timed out after {}s", self.config.data_timeout);
                "451 Timeout processing mail".to_string()
            }
        }
    }
}

fn process_sync(
    config: Config,
    sl: Arc<SimpleLoginClient>,
    mail_from: &str,
    rcpt_tos: &[String],
    data: &[u8],
) -> Result<()> {
    log::info!("Received mail from={} to={:?}", mail_from, rcpt_tos);
    let mut alias_map = HashMap::new();
    // Preserve rcpt_tos order for the outgoing envelope, matching Python's
    // dict insertion-order iteration (dicts there are ordered by first
    // insert), which HashMap does not guarantee.
    let mut new_rcpts = Vec::with_capacity(rcpt_tos.len());
    for rcpt in rcpt_tos {
        let reverse = sl.get_reverse_alias(mail_from, rcpt)?;
        log::info!("  {} -> {}", rcpt, reverse);
        new_rcpts.push(parse_single_addr(&reverse).1);
        alias_map.insert(rcpt.clone(), reverse);
    }
    let rewritten = rewrite_message(
        data,
        &alias_map,
        config.upstream_username.as_deref().unwrap(),
    );
    // original is blocking smtplib SMTP with timeout=UPSTREAM_TIMEOUT.
    let rt = tokio::runtime::Handle::current();
    rt.block_on(send_upstream(&config, mail_from, &new_rcpts, &rewritten))?;
    log::info!("Relayed successfully");
    Ok(())
}

/// Mutate To/Cc/Bcc headers and normalize each parseable From mailbox to the
/// authenticated upstream identity. No From header is added; malformed or
/// non-mailbox From fields are retained unchanged. Output line normalization
/// deliberately retains original header/body bytes except changed fields.
pub fn rewrite_message(
    data: &[u8],
    alias_map: &HashMap<String, String>,
    upstream_username: &str,
) -> Vec<u8> {
    let text = String::from_utf8_lossy(data);
    let (head, body, sep) = if let Some(i) = text.find("\r\n\r\n") {
        (&text[..i], &text[i + 4..], "\r\n\r\n")
    } else if let Some(i) = text.find("\n\n") {
        (&text[..i], &text[i + 2..], "\n\n")
    } else {
        (text.as_ref(), "", "")
    };
    let nl = if head.contains("\r\n") { "\r\n" } else { "\n" };
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in head.split(nl) {
        if (line.starts_with(' ') || line.starts_with('\t')) && !fields.is_empty() {
            fields.last_mut().unwrap().1.push(' ');
            fields.last_mut().unwrap().1.push_str(line.trim());
        } else if let Some((name, value)) = line.split_once(':') {
            fields.push((name.to_string(), value.trim_start().to_string()));
        }
    }
    let mut out_fields = Vec::new();
    for (name, value) in fields {
        if name.eq_ignore_ascii_case("Bcc") {
            continue;
        }
        if name.eq_ignore_ascii_case("To") || name.eq_ignore_ascii_case("Cc") {
            out_fields.push((name, replace_addresses(&value, alias_map)));
        } else if name.eq_ignore_ascii_case("From") {
            out_fields.push((
                name,
                normalize_from_address(&value, upstream_username).unwrap_or(value),
            ));
        } else {
            out_fields.push((name, value));
        }
    }
    let mut out = String::new();
    for (name, value) in out_fields {
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push_str(nl);
    }
    out.push_str(sep);
    out.push_str(body);
    out.into_bytes()
}

fn upstream_error_response(raw_response: &str) -> String {
    let compact: String = raw_response
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() || character == ' ' {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut words = compact.split_whitespace();
    let code = words
        .next()
        .filter(|word| word.len() == 3 && word.as_bytes().iter().all(u8::is_ascii_digit));
    let lower = compact.to_ascii_lowercase();
    let enhanced_status = words.clone().find(|word| {
        let mut parts = word.split('.');
        parts.clone().count() == 3
            && parts.all(|part| !part.is_empty() && part.as_bytes().iter().all(u8::is_ascii_digit))
    });
    // Keep only a fixed category, rather than echoing arbitrary server text:
    // upstream responses may be untrusted and must not disclose message data
    // or credentials to the SMTP client.
    let category = if lower.contains("sender") || lower.contains("mail from") {
        Some("sender rejected")
    } else if lower.contains("recipient") || lower.contains("rcpt to") {
        Some("recipient rejected")
    } else if lower.contains("message") || lower.contains("data") {
        Some("message rejected")
    } else {
        None
    };
    let message = match code {
        Some(code) => [Some(code), enhanced_status, category]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" "),
        None => String::new(),
    };
    if message.is_empty() {
        "451 Upstream SMTP error".to_string()
    } else {
        format!("451 Upstream SMTP error: {message}")
    }
}

fn upstream_error(raw_response: &str) -> anyhow::Error {
    anyhow!(upstream_error_response(raw_response)
        .trim_start_matches("451 ")
        .to_string())
}

async fn read_line<S: AsyncRead + Unpin>(s: &mut S) -> Result<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        let n = s.read(&mut b).await?;
        if n == 0 {
            return Err(anyhow!("upstream EOF"));
        }
        out.push(b[0]);
        if b[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out)
        .trim_end_matches(['\r', '\n'])
        .to_string())
}
async fn read_response<S: AsyncRead + Unpin>(s: &mut S) -> Result<String> {
    let first = read_line(s).await?;
    if first.len() >= 4 && first.as_bytes()[3] == b'-' {
        let code = &first[..3];
        loop {
            let line = read_line(s).await?;
            if line.starts_with(&format!("{} ", code)) {
                break;
            }
        }
    }
    Ok(first)
}
async fn cmd<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    text: &str,
    want: char,
) -> Result<String> {
    s.write_all(text.as_bytes()).await?;
    s.write_all(b"\r\n").await?;
    s.flush().await?;
    let line = read_response(s).await?;
    if !line.starts_with(want) {
        return Err(upstream_error(&line));
    }
    Ok(line)
}
async fn expect<S: AsyncRead + Unpin>(s: &mut S, want: char) -> Result<()> {
    let l = read_response(s).await?;
    if l.starts_with(want) {
        Ok(())
    } else {
        Err(upstream_error(&l))
    }
}

async fn send_data<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, data: &[u8]) -> Result<()> {
    cmd(s, "DATA", '3').await?;
    // smtplib's sendmail quotes lines beginning dot and terminates CRLF-dot-CRLF.
    for line in data.split_inclusive(|b| *b == b'\n') {
        if line.starts_with(b".") {
            s.write_all(b".").await?;
        }
        s.write_all(line).await?;
    }
    if !data.ends_with(b"\n") {
        s.write_all(b"\r\n").await?;
    }
    s.write_all(b".\r\n").await?;
    s.flush().await?;
    expect(s, '2').await
}
async fn smtp_auth<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, c: &Config) -> Result<()> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    cmd(
        s,
        &format!(
            "AUTH LOGIN {}",
            STANDARD.encode(c.upstream_username.as_ref().unwrap())
        ),
        '3',
    )
    .await?;
    cmd(
        s,
        &STANDARD.encode(c.upstream_password.as_ref().unwrap()),
        '2',
    )
    .await?;
    Ok(())
}

async fn smtp_send<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    c: &Config,
    mail_from: &str,
    rcpts: &[String],
    data: &[u8],
) -> Result<()> {
    expect(s, '2').await?;
    cmd(s, "EHLO localhost", '2').await?;
    smtp_auth(s, c).await?;
    cmd(s, &format!("MAIL FROM:<{}>", mail_from), '2').await?;
    for r in rcpts {
        cmd(s, &format!("RCPT TO:<{}>", r), '2').await?;
    }
    send_data(s, data).await?;
    let _ = cmd(s, "QUIT", '2').await;
    Ok(())
}

pub async fn send_upstream(
    c: &Config,
    _mail_from: &str,
    rcpts: &[String],
    data: &[u8],
) -> Result<()> {
    // The upstream envelope sender is the authenticated identity. `data` is
    // passed through unchanged here, so its RFC 5322 From header is never rewritten.
    let upstream_from = c.upstream_username.as_deref().unwrap();
    let tcp = tokio::time::timeout(
        Duration::from_secs(c.upstream_timeout),
        TcpStream::connect((c.upstream_host.as_str(), c.upstream_port)),
    )
    .await??;
    if c.upstream_starttls {
        let mut tcp = tcp;
        expect(&mut tcp, '2').await?;
        cmd(&mut tcp, "EHLO localhost", '2').await?;
        cmd(&mut tcp, "STARTTLS", '2').await?;
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()?;
        let tls = TlsConnector::from(connector)
            .connect(&c.upstream_host, tcp)
            .await?;
        // already did greeting/EHLO/pre-TLS; now only EHLO + auth/send
        let mut s = tls;
        cmd(&mut s, "EHLO localhost", '2').await?;
        smtp_auth(&mut s, c).await?;
        cmd(&mut s, &format!("MAIL FROM:<{}>", upstream_from), '2').await?;
        for r in rcpts {
            cmd(&mut s, &format!("RCPT TO:<{}>", r), '2').await?;
        }
        send_data(&mut s, data).await?;
        let _ = cmd(&mut s, "QUIT", '2').await;
        Ok(())
    } else {
        let mut s = tcp;
        smtp_send(&mut s, c, upstream_from, rcpts, data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_rejection_is_a_sanitized_single_line_451_response() {
        let response = upstream_error_response(
            "501 5.5.4 sender rejected\r\n250 injected response token=not-for-clients",
        );
        assert_eq!(
            response,
            "451 Upstream SMTP error: 501 5.5.4 sender rejected"
        );
        assert!(!response.contains('\r'));
        assert!(!response.contains('\n'));
        assert!(!response.contains("injected"));
        assert!(!response.contains("not-for-clients"));
    }

    #[test]
    fn preserves_inbound_sender_for_reverse_alias_lookup_and_rewrites_final_upstream_identities() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener as StdTcpListener;
        use std::sync::mpsc;

        fn capture_simplelogin_api() -> (u16, mpsc::Receiver<String>) {
            let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let (requests_tx, requests_rx) = mpsc::channel();
            std::thread::spawn(move || {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    while {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        line != "\r\n"
                    } {}
                    requests_tx.send(request_line.clone()).unwrap();
                    let mut writer = stream;
                    let body = if request_line.starts_with("GET ") {
                        r#"{"aliases":[{"id":7,"email":"original@app.example.test"}]}"#
                    } else {
                        r#"{"reverse_alias":"reverse@simplelogin.example.test"}"#
                    };
                    write!(
                        writer,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                    writer.flush().unwrap();
                }
            });
            (port, requests_rx)
        }

        async fn capture_upstream() -> (u16, tokio::task::JoinHandle<(String, String)>) {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                writer.write_all(b"220 mock\r\n").await.unwrap();
                writer.flush().await.unwrap();
                let mut envelope = String::new();
                let mut message = String::new();
                let mut auth_step = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line.starts_with("EHLO") {
                        writer.write_all(b"250 mock\r\n").await.unwrap();
                    } else if line.starts_with("AUTH LOGIN") {
                        auth_step = 1;
                        writer.write_all(b"334 password\r\n").await.unwrap();
                    } else if auth_step == 1 {
                        auth_step = 0;
                        writer.write_all(b"235 authenticated\r\n").await.unwrap();
                    } else if line.starts_with("MAIL FROM:") {
                        envelope = line.trim().to_string();
                        writer.write_all(b"250 sender accepted\r\n").await.unwrap();
                    } else if line.starts_with("RCPT TO:") {
                        writer
                            .write_all(b"250 recipient accepted\r\n")
                            .await
                            .unwrap();
                    } else if line.trim() == "DATA" {
                        writer.write_all(b"354 continue\r\n").await.unwrap();
                        loop {
                            line.clear();
                            reader.read_line(&mut line).await.unwrap();
                            if line == ".\r\n" {
                                break;
                            }
                            message.push_str(&line);
                        }
                        writer.write_all(b"250 queued\r\n").await.unwrap();
                    } else if line.trim() == "QUIT" {
                        writer.write_all(b"221 bye\r\n").await.unwrap();
                        writer.flush().await.unwrap();
                        return (envelope, message);
                    }
                    writer.flush().await.unwrap();
                }
            });
            (port, server)
        }

        let (api_port, api_requests) = capture_simplelogin_api();
        let (upstream_port, upstream) = capture_upstream().await;
        let config = Config {
            relay_host: "127.0.0.1".into(),
            relay_port: 0,
            relay_username: Some("relay".into()),
            relay_password: Some("relay-pass".into()),
            tls_enabled: false,
            tls_cert: String::new(),
            tls_key: String::new(),
            sl_api_url: format!("http://127.0.0.1:{api_port}"),
            sl_api_key: Some("test-key".into()),
            upstream_host: "127.0.0.1".into(),
            upstream_port,
            upstream_username: Some("upstream@example.test".into()),
            upstream_password: Some("upstream-pass".into()),
            upstream_starttls: false,
            data_timeout: 30,
            upstream_timeout: 5,
            log_level: "ERROR".into(),
        };
        let sl_url = config.sl_api_url.clone();
        let sl = std::thread::spawn(move || SimpleLoginClient::new(&sl_url, "test-key"))
            .join()
            .unwrap()
            .unwrap();
        // reqwest's blocking client owns an internal runtime; retain it until
        // process exit so it is never dropped while this async test is polling.
        let sl = Box::leak(Box::new(Arc::new(sl))).clone();
        let handler = RelayHandler::new(config, sl);
        let response = handler
            .process(
                "original@app.example.test".into(),
                vec!["recipient@example.test".into()],
                b"From: Visible Sender <visible@example.test>\r\nTo: recipient@example.test\r\n\r\nBody\r\n".to_vec(),
            )
            .await;

        assert_eq!(response, "250 OK");
        let lookup = api_requests.recv().unwrap();
        assert!(
            lookup.contains("query=original%40app.example.test")
                || lookup.contains("query=original@app.example.test"),
            "reverse-alias lookup did not receive the inbound envelope sender: {lookup}"
        );
        let (envelope, message) = upstream.await.unwrap();
        assert_eq!(envelope, "MAIL FROM:<upstream@example.test>");
        assert!(message.contains("From: Visible Sender <upstream@example.test>"));
        });
    }

    #[test]
    fn named_from_is_normalized_to_upstream_identity() {
        let rewritten = rewrite_message(
            b"From: Alerts <old@example.test>\r\nTo: recipient@example.test\r\n\r\nBody\r\n",
            &HashMap::new(),
            "upstream@example.test",
        );
        assert!(String::from_utf8(rewritten)
            .unwrap()
            .contains("From: Alerts <upstream@example.test>"));
    }

    #[test]
    fn no_from_header_is_not_injected() {
        let rewritten = rewrite_message(
            b"To: recipient@example.test\r\nSubject: unchanged\r\n\r\nBody\r\n",
            &HashMap::new(),
            "upstream@example.test",
        );
        let text = String::from_utf8(rewritten).unwrap();
        assert!(!text.to_ascii_lowercase().contains("from:"));
        assert!(text.contains("To: recipient@example.test"));
    }

    #[test]
    fn bcc_is_stripped_to_and_cc_rewritten() {
        let mut m = HashMap::new();
        m.insert("to@example.com".into(), "ra@sl.test".into());
        m.insert("cc@example.com".into(), "rb@sl.test".into());
        let x=rewrite_message(b"From: sender@test\r\nTo: Person <to@example.com>\r\nCc: cc@example.com\r\nBcc: hidden@example.com\r\nSubject: test\r\n\r\nbody\r\n",&m,"upstream@example.test");
        let s = String::from_utf8(x).unwrap();
        assert!(s.contains("To: Person <ra@sl.test>"));
        assert!(s.contains("Cc: rb@sl.test"));
        assert!(!s.to_lowercase().contains("bcc:"));
        assert!(s.ends_with("body\r\n"));
    }
}
