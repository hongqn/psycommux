use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde_json::json;

use psycommux::cmux::{default_socket_path, CmuxClient};
use psycommux::events::EventHub;
use psycommux::net::{self, LOOPBACK_HOSTS, WILDCARD_HOSTS};
use psycommux::web::{router, AppState};

/// Drive cmux workspaces from a phone or another browser.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Bind address (expose remotely through Tailscale, a TLS proxy, or --tls-cert)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// cmux socket path (default: $CMUX_SOCKET_PATH, then cmux's stable socket)
    #[arg(long)]
    socket: Option<String>,
    /// Extra Origin allowed for POSTs, e.g. https://mac.tailnet.ts.net
    #[arg(long = "allow-origin")]
    allow_origin: Vec<String>,
    /// URL other devices use to reach this server, e.g. https://mac.tailnet.ts.net;
    /// printed as a QR code (default: found from `tailscale serve`, or --host)
    #[arg(long)]
    public_url: Option<String>,
    /// Serve HTTPS with this certificate (PEM; may also hold the key),
    /// e.g. from `tailscale cert <mac>.<tailnet>.ts.net`
    #[arg(long)]
    tls_cert: Option<PathBuf>,
    /// Private key for --tls-cert (PEM; default: the cert's name with .key, if it exists)
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Do not print a QR code of the login URL
    #[arg(long)]
    no_qr: bool,
    /// Log every request, not only errors (avoid when the server's own terminal is watched)
    #[arg(long)]
    access_log: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Args::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let tls = match &args.tls_cert {
        Some(cert) => {
            let key = args.tls_key.clone().or_else(|| net::default_tls_key(cert));
            Some(
                net::load_tls_config(cert, key.as_deref())
                    .map_err(|e| format!("cannot load TLS certificate: {e}"))?,
            )
        }
        None => None,
    };

    let token = std::env::var("PSYCOMMUX_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(net::new_token);
    let password = std::env::var("CMUX_SOCKET_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());
    let client = Arc::new(CmuxClient::new(
        args.socket.clone().unwrap_or_else(default_socket_path),
        password,
    ));

    if let Err(e) = client.call("system.ping", json!({})).await {
        if e.code == "access_denied" || e.code == "auth_failed" {
            return Err(e.message);
        }
        eprintln!("warning: cmux socket check failed ({e}); will keep retrying");
    }

    let tls_name = args.tls_cert.as_deref().and_then(net::cert_hostname);
    let serve_url = if args.public_url.is_some() || tls.is_some() {
        None
    } else {
        net::detect_tailscale_serve(args.port).await
    };
    let scheme = if tls.is_some() { "https" } else { "http" };
    let remote = net::phone_url(
        &args.host,
        args.port,
        args.public_url.as_deref(),
        serve_url.as_deref(),
        tls_name.as_deref(),
        scheme,
    );
    let mut origins = args.allow_origin.clone();
    if let Some(remote) = remote.as_deref().filter(|r| r.starts_with("https://")) {
        origins.push(net::origin_of(remote)); // proxies may rewrite Host
    }

    let hub = EventHub::new(Arc::clone(&client));
    hub.start();
    let mut state = AppState::new(Arc::clone(&client), hub, token.clone());
    state.allowed_origins = origins;
    state.access_log = args.access_log;
    state.tls = tls.is_some();
    let app = router(Arc::new(state));

    let bind_host = if args.host.contains(':') {
        format!("[{}]", args.host)
    } else {
        args.host.clone()
    };
    let listener = tokio::net::TcpListener::bind(format!("{bind_host}:{}", args.port))
        .await
        .map_err(|e| format!("cannot listen on {}:{}: {e}", args.host, args.port))?;

    if !LOOPBACK_HOSTS.contains(&args.host.as_str()) && tls.is_none() {
        eprintln!("warning: listening beyond loopback without TLS; anyone who gets the token can type into your terminals");
    }
    println!("cmux socket: {}", client.socket_path);
    let local_host = if WILDCARD_HOSTS.contains(&args.host.as_str()) {
        "127.0.0.1"
    } else {
        bind_host.as_str()
    };
    println!(
        "open: {scheme}://{local_host}:{}/login?token={token}",
        args.port
    );
    match &remote {
        Some(remote) => {
            let login = format!("{remote}/login?token={token}");
            println!("phone: {login}");
            if !args.no_qr && std::io::stdout().is_terminal() {
                match net::qr_render(&login) {
                    Ok(qr) => println!("{qr}"),
                    Err(e) => eprintln!("warning: {e}"),
                }
            }
        }
        None if WILDCARD_HOSTS.contains(&args.host.as_str()) => {
            println!("phone: pass --public-url <url other devices use> to print a QR code");
        }
        None => {}
    }

    match tls {
        None => axum::serve(listener, app).await.map_err(|e| e.to_string()),
        Some(config) => net::serve_tls(listener, app, config).await,
    }
}
