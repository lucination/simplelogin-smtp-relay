mod auth;
mod config;
mod handler;
mod mail;
mod simplelogin_client;

use auth::{auth_plain, decode_login_step, validate_credentials, AuthResult};
use config::Config;
use handler::RelayHandler;
use simplelogin_client::SimpleLoginClient;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_native_tls::TlsAcceptor;

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        std::process::exit(healthcheck());
    }
    let config = Config::from_env();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&config.log_level))
        .init();
    if let Err(missing) = config.validate() {
        if missing == vec!["TLS_CERT_OR_KEY"] {
            log::error!("TLS_ENABLED=true but TLS_CERT or TLS_KEY not set");
        } else {
            log::error!("Missing required env vars: {}", missing.join(", "));
        }
        std::process::exit(1);
    }
    let sl = match std::thread::spawn({
        let url = config.sl_api_url.clone();
        let key = config.sl_api_key.clone().unwrap();
        move || SimpleLoginClient::new(&url, &key)
    })
    .join()
    {
        Ok(Ok(x)) => Arc::new(x),
        Ok(Err(e)) => {
            log::error!("Failed creating SimpleLogin client: {e}");
            std::process::exit(1)
        }
        Err(_) => {
            log::error!("Failed creating SimpleLogin client thread");
            std::process::exit(1)
        }
    };
    let handler = Arc::new(RelayHandler::new(config.clone(), sl));
    let listener = match TcpListener::bind((config.relay_host.as_str(), config.relay_port)).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("bind failed: {e}");
            std::process::exit(1)
        }
    };
    let tls_acceptor = if config.tls_enabled {
        match load_tls(&config) {
            Ok(x) => Some(x),
            Err(e) => {
                log::error!("TLS setup failed: {e}");
                std::process::exit(1)
            }
        }
    } else {
        None
    };
    log::info!("Listening on {}:{}", config.relay_host, config.relay_port);
    log::info!(
        "Upstream: {}:{} (STARTTLS={})",
        config.upstream_host,
        config.upstream_port,
        if config.upstream_starttls {
            "on"
        } else {
            "off"
        }
    );
    log::info!(
        "TLS: {}",
        if config.tls_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    log::info!(
        "Timeouts: DATA={}s UPSTREAM={}s",
        config.data_timeout,
        config.upstream_timeout
    );
    loop {
        tokio::select! {
            r=listener.accept()=>match r {Ok((s,_))=>{let c=config.clone();let h=Arc::clone(&handler);let a=tls_acceptor.clone();tokio::spawn(async move {if let Err(e)=serve_tcp(s,c,h,a).await {log::debug!("connection: {e}")}});},Err(e)=>log::error!("accept: {e}")},
            _=tokio::signal::ctrl_c()=>{log::info!("Received shutdown signal, shutting down...");break;}
        }
    }
    log::info!("Shutdown complete");
}

/// Standalone healthcheck mode: connects to the relay's own listener
/// (127.0.0.1:$RELAY_PORT, default 8025) and confirms it responds with an
/// SMTP "2xx" greeting banner. Intended for `HEALTHCHECK CMD ["/app/smtp-relay", "--healthcheck"]`
/// in Docker so the image needs no extra tools (nc, bash /dev/tcp, etc).
/// Returns a process exit code: 0 = healthy, 1 = unhealthy.
fn healthcheck() -> i32 {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    let port = std::env::var("RELAY_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8025);
    let addr = format!("127.0.0.1:{port}");

    let mut stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(_) => return 1,
    };
    if stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .is_err()
    {
        return 1;
    }
    let mut buf = [0u8; 8];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 && buf[0] == b'2' => 0,
        _ => 1,
    }
}

fn load_tls(c: &Config) -> anyhow::Result<TlsAcceptor> {
    let identity = native_tls::Identity::from_pkcs8(
        &std::fs::read(&c.tls_cert)?,
        &std::fs::read(&c.tls_key)?,
    )?;
    Ok(TlsAcceptor::from(
        native_tls::TlsAcceptor::builder(identity).build()?,
    ))
}

async fn send<S: AsyncWrite + Unpin>(s: &mut S, line: &str) -> anyhow::Result<()> {
    s.write_all(line.as_bytes()).await?;
    s.write_all(b"\r\n").await?;
    s.flush().await?;
    Ok(())
}

async fn serve_tcp(
    stream: TcpStream,
    c: Config,
    h: Arc<RelayHandler>,
    tls: Option<TlsAcceptor>,
) -> anyhow::Result<()> {
    if tls.is_none() {
        return serve_session(stream, c, h, false).await;
    }
    // STARTTLS is only advertised/accepted when TLS_ENABLED. Before upgrade,
    // Python aiosmtpd requires TLS for AUTH and mail transactions.
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut writer = w;
    send(&mut writer, "220 simplelogin-smtp-relay ESMTP").await?;
    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf).await? == 0 {
            return Ok(());
        };
        let line = buf.trim_end_matches(['\r', '\n']);
        let upper = line.to_uppercase();
        if upper == "STARTTLS" && tls.is_some() {
            send(&mut writer, "220 Ready to start TLS").await?;
            let r = reader.into_inner();
            let w = writer.reunite(r)?;
            let encrypted = tls.unwrap().accept(w).await?;
            return serve_session(encrypted, c, h, true).await;
        }
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            if tls.is_some() {
                send(&mut writer, "250-localhost").await?;
                send(&mut writer, "250-STARTTLS").await?;
                send(&mut writer, "250 AUTH LOGIN PLAIN").await?;
            } else {
                send(&mut writer, "250-localhost").await?;
                send(&mut writer, "250 AUTH LOGIN PLAIN").await?;
            }
            continue;
        }
        match upper.as_str() {
            "QUIT" => {
                send(&mut writer, "221 Bye").await?;
                return Ok(());
            }
            _ => send(&mut writer, "530 Must issue STARTTLS first").await?,
        }
    }
}

async fn serve_session<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    c: Config,
    h: Arc<RelayHandler>,
    secure: bool,
) -> anyhow::Result<()> {
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);
    send(&mut w, "220 simplelogin-smtp-relay ESMTP").await?;
    let mut authed = false;
    let mut mail_from = String::new();
    let mut rcpts: Vec<String> = vec![];
    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf).await? == 0 {
            return Ok(());
        };
        let line = buf.trim_end_matches(['\r', '\n']).to_string();
        let up = line.to_uppercase();
        if up.starts_with("EHLO") || up.starts_with("HELO") {
            send(&mut w, "250-localhost").await?;
            send(&mut w, "250 AUTH LOGIN PLAIN").await?;
            continue;
        }
        if up.starts_with("AUTH PLAIN") {
            let arg = line.splitn(3, ' ').nth(2).unwrap_or("");
            if auth_plain(
                arg,
                c.relay_username.as_ref().unwrap(),
                c.relay_password.as_ref().unwrap(),
            ) == AuthResult::Success
            {
                authed = true;
                send(&mut w, "235 2.7.0 Authentication successful").await?
            } else {
                send(&mut w, "535 5.7.8 Authentication credentials invalid").await?
            };
            continue;
        }
        if up == "AUTH LOGIN" || up.starts_with("AUTH LOGIN ") {
            if !secure {
                send(
                    &mut w,
                    "538 Encryption required for requested authentication mechanism",
                )
                .await?;
                continue;
            };
            let first = line.splitn(3, ' ').nth(2);
            send(&mut w, "334 VXNlcm5hbWU6").await?;
            let user = match first {
                Some(v) => decode_login_step(v).unwrap_or_default(),
                None => {
                    buf.clear();
                    reader.read_line(&mut buf).await?;
                    decode_login_step(buf.trim()).unwrap_or_default()
                }
            };
            send(&mut w, "334 UGFzc3dvcmQ6").await?;
            buf.clear();
            reader.read_line(&mut buf).await?;
            let pass = decode_login_step(buf.trim()).unwrap_or_default();
            if validate_credentials(
                &user,
                &pass,
                c.relay_username.as_ref().unwrap(),
                c.relay_password.as_ref().unwrap(),
            ) == AuthResult::Success
            {
                authed = true;
                send(&mut w, "235 2.7.0 Authentication successful").await?
            } else {
                send(&mut w, "535 5.7.8 Authentication credentials invalid").await?
            };
            continue;
        }
        if up == "QUIT" {
            send(&mut w, "221 Bye").await?;
            return Ok(());
        }
        if !authed {
            send(&mut w, "530 5.7.0 Authentication required").await?;
            continue;
        }
        if up.starts_with("MAIL FROM:") {
            mail_from = extract_path(&line);
            rcpts.clear();
            send(&mut w, "250 OK").await?;
            continue;
        }
        if up.starts_with("RCPT TO:") {
            rcpts.push(extract_path(&line));
            send(&mut w, "250 OK").await?;
            continue;
        }
        if up == "DATA" {
            if mail_from.is_empty() || rcpts.is_empty() {
                send(&mut w, "503 Bad sequence of commands").await?;
                continue;
            };
            send(&mut w, "354 End data with <CR><LF>.<CR><LF>").await?;
            let mut data = Vec::new();
            loop {
                buf.clear();
                reader.read_line(&mut buf).await?;
                if buf == ".\r\n" || buf == ".\n" {
                    break;
                };
                let s = if buf.starts_with("..") {
                    &buf[1..]
                } else {
                    &buf
                };
                data.extend_from_slice(s.as_bytes())
            }
            let response = h.process(mail_from.clone(), rcpts.clone(), data).await;
            send(&mut w, &response).await?;
            continue;
        }
        send(&mut w, "500 Error: command not recognized").await?;
    }
}
fn extract_path(s: &str) -> String {
    s.split_once(':')
        .map(|(_, v)| v.trim().trim_matches(['<', '>']).to_string())
        .unwrap_or_default()
}
