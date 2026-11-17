//! Reaching the server from a phone: TLS, Tailscale Serve, the login URL and
//! its QR code, and the Home Screen icon.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

pub const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];
pub const WILDCARD_HOSTS: [&str; 3] = ["0.0.0.0", "::", ""];

/// The key `tailscale cert` writes next to the cert (name.crt -> name.key), unless the cert holds one.
pub fn default_tls_key(cert: &Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read(cert).ok()?;
    if String::from_utf8_lossy(&text).contains("PRIVATE KEY-----") {
        return None;
    }
    let stem = cert.with_extension("key");
    let appended = std::path::PathBuf::from(format!("{}.key", cert.display()));
    [stem, appended].into_iter().find(|p| p.is_file())
}

pub fn load_tls_config(
    cert: &Path,
    key: Option<&Path>,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let cert_pem = std::fs::read(cert).map_err(|e| format!("{}: {e}", cert.display()))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{} holds no PEM certificate", cert.display()));
    }
    let key_der = match key {
        Some(key) => {
            let pem = std::fs::read(key).map_err(|e| format!("{}: {e}", key.display()))?;
            rustls_pemfile::private_key(&mut pem.as_slice())
                .map_err(|e| format!("{}: {e}", key.display()))?
                .ok_or_else(|| format!("{} holds no PEM private key", key.display()))?
        }
        None => rustls_pemfile::private_key(&mut cert_pem.as_slice())
            .ok()
            .flatten()
            .ok_or_else(|| {
                format!(
                    "{} holds no private key; pass it with --tls-key",
                    cert.display()
                )
            })?,
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .map_err(|e| {
            let key = key
                .map(|k| k.display().to_string())
                .unwrap_or_else(|| cert.display().to_string());
            format!(
                "{} and {key} are not a matching PEM certificate and key ({e})",
                cert.display()
            )
        })?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// First non-wildcard DNS name in the certificate, e.g. mac.tailnet.ts.net from `tailscale cert`.
pub fn cert_hostname(cert: &Path) -> Option<String> {
    let data = std::fs::read(cert).ok()?;
    for pem in x509_parser::pem::Pem::iter_from_buffer(&data) {
        let Ok(pem) = pem else { continue };
        if pem.label != "CERTIFICATE" {
            continue;
        }
        let Ok(x509) = pem.parse_x509() else { continue };
        let Ok(Some(san)) = x509.subject_alternative_name() else {
            return None;
        };
        return san.value.general_names.iter().find_map(|name| match name {
            x509_parser::extensions::GeneralName::DNSName(dns) if !dns.contains('*') => {
                Some(dns.to_string())
            }
            _ => None,
        });
    }
    None
}

/// The https://<name>.ts.net URL whose Tailscale Serve handler proxies to our port, if any.
///
/// `status` is the parsed output of `tailscale serve status --json`.
pub fn tailscale_serve_url(port: u16, status: &Value) -> Option<String> {
    for (host_port, web) in status.get("Web")?.as_object()? {
        let Some(handlers) = web.get("Handlers").and_then(Value::as_object) else {
            continue;
        };
        for handler in handlers.values() {
            let target = handler.get("Proxy").and_then(Value::as_str).unwrap_or("");
            let target = if target.contains("://") {
                target.to_string()
            } else {
                format!("http://{target}")
            };
            let Ok(parsed) = url::Url::parse(&target) else {
                continue;
            };
            let host = parsed
                .host_str()
                .unwrap_or("")
                .trim_start_matches('[')
                .trim_end_matches(']');
            if LOOPBACK_HOSTS.contains(&host) && parsed.port_or_known_default() == Some(port) {
                let (name, https_port) = host_port.rsplit_once(':').unwrap_or((host_port, "443"));
                let suffix = if https_port == "443" {
                    String::new()
                } else {
                    format!(":{https_port}")
                };
                return Some(format!("https://{name}{suffix}"));
            }
        }
    }
    None
}

pub async fn detect_tailscale_serve(port: u16) -> Option<String> {
    let run = tokio::process::Command::new("tailscale")
        .args(["serve", "status", "--json"])
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(3), run)
        .await
        .ok()?
        .ok()?;
    let status: Value = serde_json::from_slice(&out.stdout).ok()?;
    tailscale_serve_url(port, &status)
}

/// The base URL another device should open, or None when only this Mac can reach the server.
///
/// `tls_name` is the certificate's hostname when serving TLS ourselves; the
/// phone has to use that name, not the bind address, for the certificate to match.
pub fn phone_url(
    host: &str,
    port: u16,
    public_url: Option<&str>,
    serve_url: Option<&str>,
    tls_name: Option<&str>,
    scheme: &str,
) -> Option<String> {
    if let Some(url) = public_url {
        return Some(url.trim_end_matches('/').to_string());
    }
    if let Some(name) = tls_name {
        return Some(if port == 443 {
            format!("https://{name}")
        } else {
            format!("https://{name}:{port}")
        });
    }
    if let Some(url) = serve_url {
        return Some(url.to_string());
    }
    if LOOPBACK_HOSTS.contains(&host) || WILDCARD_HOSTS.contains(&host) {
        return None;
    }
    Some(if host.contains(':') {
        format!("{scheme}://[{host}]:{port}")
    } else {
        format!("{scheme}://{host}:{port}")
    })
}

/// `host[:port]` of a URL, as written (Python's urlparse().netloc minus userinfo).
pub fn netloc(url: &str) -> &str {
    let rest = url.split_once("://").map_or("", |(_, rest)| rest);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or("");
    rest.rsplit_once('@').map_or(rest, |(_, host)| host)
}

pub fn origin_of(url: &str) -> String {
    let scheme = url.split_once("://").map_or("", |(scheme, _)| scheme);
    format!("{scheme}://{}", netloc(url))
}

/// Black-on-white QR code, two modules per character cell, so it scans on dark themes too.
pub fn qr_render(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
        .map_err(|e| format!("cannot make a QR code: {e:?}"))?;
    let n = code.width();
    let colors = code.to_colors();
    let quiet = 4usize;
    let size = n + quiet * 2;
    let at = |x: usize, y: usize| {
        x >= quiet
            && y >= quiet
            && x - quiet < n
            && y - quiet < n
            && colors[(y - quiet) * n + (x - quiet)] == qrcode::Color::Dark
    };
    let mut rows = Vec::new();
    for y in (0..size).step_by(2) {
        let line: String = (0..size)
            .map(|x| match (at(x, y), y + 1 < size && at(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            })
            .collect();
        rows.push(format!("\x1b[38;5;16;48;5;231m{line}\x1b[0m"));
    }
    Ok(rows.join("\n"))
}

/// The psycommu: a glowing core (the phone) inside its thought waves, with a
/// ring of funnels (the agents) flying around it. Drawn here so the project
/// ships no binary files; every pixel averages a 4x4 grid of samples.
pub fn icon_png(size: u32) -> Vec<u8> {
    const ACCENT: [f64; 3] = [217.0, 119.0, 87.0];
    const FUNNELS: usize = 6;
    const ORBIT: f64 = 0.33;
    const TRAIL: f64 = 0.75; // radians of orbit behind each funnel
    let s = size as f64;
    let start = -std::f64::consts::FRAC_PI_2;
    let step = std::f64::consts::TAU / FUNNELS as f64;
    // Shapes in unit coordinates, centered on (0, 0), y down.
    let funnels: Vec<[(f64, f64); 3]> = (0..FUNNELS)
        .map(|i| {
            let angle = start + i as f64 * step;
            let (cx, cy) = (ORBIT * angle.cos(), ORBIT * angle.sin());
            // Flying clockwise along the orbit, nose turned a little outward.
            let heading = angle + std::f64::consts::FRAC_PI_2 - 0.35;
            let (hx, hy) = (heading.cos(), heading.sin());
            let (len, base) = (0.068, 0.034);
            [
                (cx + hx * len, cy + hy * len),
                (
                    cx - hx * len * 0.8 - hy * base,
                    cy - hy * len * 0.8 + hx * base,
                ),
                (
                    cx - hx * len * 0.8 + hy * base,
                    cy - hy * len * 0.8 - hx * base,
                ),
            ]
        })
        .collect();
    let in_triangle = |x: f64, y: f64, t: &[(f64, f64); 3]| {
        let side = |(ax, ay): (f64, f64), (bx, by): (f64, f64)| {
            (bx - ax) * (y - ay) - (by - ay) * (x - ax)
        };
        let (a, b, c) = (side(t[0], t[1]), side(t[1], t[2]), side(t[2], t[0]));
        (a >= 0.0 && b >= 0.0 && c >= 0.0) || (a <= 0.0 && b <= 0.0 && c <= 0.0)
    };
    // Color of one sample point.
    let sample = |x: f64, y: f64| -> [f64; 3] {
        let r = (x * x + y * y).sqrt();
        // Background: warm dark, a little lighter toward the middle, plus the core's glow.
        let shade = 30.0 + 14.0 * (1.0 - (r / 0.7).min(1.0));
        let glow = (-(r / 0.16).powi(2)).exp() * 0.55;
        let mut c = [
            shade + (ACCENT[0] - shade) * glow,
            shade * 0.97 + (ACCENT[1] - shade) * glow,
            shade * 0.94 + (ACCENT[2] - shade) * glow,
        ];
        let mut over = |rgb: [f64; 3], alpha: f64| {
            for k in 0..3 {
                c[k] += (rgb[k] - c[k]) * alpha;
            }
        };
        for (radius, alpha) in [(0.15, 0.5), (0.21, 0.28)] {
            if (r - radius).abs() <= 0.008 {
                over(ACCENT, alpha);
            }
        }
        // Each funnel's trail: an arc of the orbit behind it, thinning and fading.
        let theta = y.atan2(x);
        for i in 0..FUNNELS {
            let behind = (start + i as f64 * step - theta).rem_euclid(std::f64::consts::TAU);
            if behind < TRAIL {
                let left = 1.0 - behind / TRAIL;
                if (r - ORBIT).abs() <= 0.016 * left {
                    over(ACCENT, 0.6 * left);
                }
            }
        }
        if funnels.iter().any(|t| in_triangle(x, y, t)) {
            over(ACCENT, 1.0);
        }
        if r <= 0.085 {
            // The core, hot white in the middle.
            let heat = (1.0 - r / 0.085).powf(1.5);
            over([255.0, 236.0, 222.0], heat * 0.85);
            over(ACCENT, 1.0 - heat * 0.85);
        }
        c
    };
    const GRID: usize = 4;
    let mut raw = Vec::with_capacity((size * (size * 3 + 1)) as usize);
    for py in 0..size {
        raw.push(0);
        for px in 0..size {
            let mut sum = [0.0; 3];
            for sy in 0..GRID {
                for sx in 0..GRID {
                    let x = (px as f64 + (sx as f64 + 0.5) / GRID as f64) / s - 0.5;
                    let y = (py as f64 + (sy as f64 + 0.5) / GRID as f64) / s - 0.5;
                    let c = sample(x, y);
                    for k in 0..3 {
                        sum[k] += c[k];
                    }
                }
            }
            for v in sum {
                raw.push((v / (GRID * GRID) as f64).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&raw).unwrap();
    let idat = z.finish().unwrap();
    let chunk = |kind: &[u8], data: &[u8]| {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(&crc.finalize().to_be_bytes());
        out
    };
    let mut header = size.to_be_bytes().to_vec();
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(chunk(b"IHDR", &header));
    png.extend(chunk(b"IDAT", &idat));
    png.extend(chunk(b"IEND", b""));
    png
}

/// POSIX shell quoting, as Python's shlex.quote.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return s.into();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// A random URL-safe login token (24 bytes of entropy).
pub fn new_token() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 24];
    getrandom(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn getrandom(buf: &mut [u8]) {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .expect("cannot read /dev/urandom");
}

/// Compare secrets without leaking where they differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Accept HTTPS connections; each handshakes in its own task, so a slow or
/// plain-HTTP client cannot stall the others.
pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    config: Arc<rustls::ServerConfig>,
) -> Result<(), String> {
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        let (tcp, _peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(Ok(tls)) =
                tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(tcp))
                    .await
            else {
                return;
            };
            let service = hyper_util::service::TowerToHyperService::new(app);
            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let _ = builder
                .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), service)
                .await;
        });
    }
}
