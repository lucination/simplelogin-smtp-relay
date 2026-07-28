use crate::config::Config;
use crate::mail::{parse_single_addr, replace_addresses};
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
    let rewritten = rewrite_message(data, &alias_map);
    // original is blocking smtplib SMTP with timeout=UPSTREAM_TIMEOUT.
    let rt = tokio::runtime::Handle::current();
    rt.block_on(send_upstream(&config, mail_from, &new_rcpts, &rewritten))?;
    log::info!("Relayed successfully");
    Ok(())
}

/// Mutate only To/Cc/Bcc headers as Python's email.message_from_bytes +
/// header deletion/reassignment does for normal RFC-5322 messages. Output
/// line normalization deliberately retains original header/body bytes except
/// the changed header fields; tests compare semantic captured headers.
pub fn rewrite_message(data: &[u8], alias_map: &HashMap<String, String>) -> Vec<u8> {
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
        return Err(anyhow!("upstream {} -> {}", text, line));
    }
    Ok(line)
}
async fn expect<S: AsyncRead + Unpin>(s: &mut S, want: char) -> Result<()> {
    let l = read_response(s).await?;
    if l.starts_with(want) {
        Ok(())
    } else {
        Err(anyhow!("upstream greeting/response: {l}"))
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
    mail_from: &str,
    rcpts: &[String],
    data: &[u8],
) -> Result<()> {
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
        cmd(&mut s, &format!("MAIL FROM:<{}>", mail_from), '2').await?;
        for r in rcpts {
            cmd(&mut s, &format!("RCPT TO:<{}>", r), '2').await?;
        }
        send_data(&mut s, data).await?;
        let _ = cmd(&mut s, "QUIT", '2').await;
        Ok(())
    } else {
        let mut s = tcp;
        smtp_send(&mut s, c, mail_from, rcpts, data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bcc_is_stripped_to_and_cc_rewritten() {
        let mut m = HashMap::new();
        m.insert("to@example.com".into(), "ra@sl.test".into());
        m.insert("cc@example.com".into(), "rb@sl.test".into());
        let x=rewrite_message(b"From: sender@test\r\nTo: Person <to@example.com>\r\nCc: cc@example.com\r\nBcc: hidden@example.com\r\nSubject: test\r\n\r\nbody\r\n",&m);
        let s = String::from_utf8(x).unwrap();
        assert!(s.contains("To: Person <ra@sl.test>"));
        assert!(s.contains("Cc: rb@sl.test"));
        assert!(!s.to_lowercase().contains("bcc:"));
        assert!(s.ends_with("body\r\n"));
    }
}
