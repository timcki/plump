// wifi upload server: HTTP file upload + mDNS (plump.local)

use alloc::string::String;
use core::fmt::Write as FmtWrite;

use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::IpListenEndpoint;
use embassy_net::Ipv4Address;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write as AsyncWrite;
use esp_hal::delay::Delay;
use esp_radio::wifi::{ClientConfig, Config, ModeConfig};
use log::{debug, info, warn};

use crate::board::action::{Action, ActionEvent, ButtonMapper};
use crate::board::{Epd, SCREEN_H, SCREEN_W};
use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::fonts::bitmap::BitmapFont;
use crate::kernel::config::WifiConfig;
use crate::kernel::tasks;
use crate::ui::{
    Alignment, BitmapLabel, ButtonFeedback, CONTENT_TOP, LARGE_MARGIN, Region, stack_fmt,
};

const HEADING_X: u16 = LARGE_MARGIN;
const HEADING_W: u16 = SCREEN_W - HEADING_X * 2;

const BODY_X: u16 = 24;
const BODY_W: u16 = SCREEN_W - BODY_X * 2;
const BODY_LINE_GAP: u16 = 10;
const FOOTER_Y: u16 = SCREEN_H - 60;

const HTTP_200_HTML: &[u8] =
    b"HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n";
const HTTP_200_JSON: &[u8] =
    b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n";
const HTTP_200_TEXT: &[u8] =
    b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n";
const HTTP_500_TEXT: &[u8] =
    b"HTTP/1.0 500 Internal Server Error\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n";
const HTTP_404: &[u8] = b"HTTP/1.0 404 Not Found\r\nConnection: close\r\n\r\nNot Found";

const UPLOAD_PAGE: &[u8] = include_bytes!("../../assets/upload.html");

const MDNS_PORT: u16 = 5353;

// "plump.local" in DNS wire format: length-prefixed labels + NUL
const HOSTNAME_WIRE: [u8; 13] = [
    5, b'p', b'l', b'u', b'm', b'p', //
    5, b'l', b'o', b'c', b'a', b'l', //
    0,
];

const MDNS_MULTICAST: [u8; 4] = [224, 0, 0, 251];

const MAX_BOUNDARY_LEN: usize = 120;
const WORK_BUF_SIZE: usize = 4096;

// TCP buffer sizes

const TCP_RX_BUF_SIZE: usize = 4096;
const TCP_TX_BUF_SIZE: usize = 2048;

const HTTP_HEADER_BUF_SIZE: usize = 1024;

const DIR_LIST_MAX: usize = 64;

// HTTP timing
const HTTP_TIMEOUT_SECS: u64 = 30;
const ACCEPT_RETRY_MS: u64 = 200;

const SOCKET_CLOSE_DELAY_MS: u64 = 10;

/// Cursor-based DNS packet writer.  Appends bytes, u16, u32 without
/// hardcoded offsets — if the hostname length changes, all downstream
/// fields shift automatically.
struct DnsBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> DnsBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn put(&mut self, data: &[u8]) {
        let n = data.len().min(self.buf.len() - self.pos);
        self.buf[self.pos..self.pos + n].copy_from_slice(&data[..n]);
        self.pos += n;
    }

    fn put_u16(&mut self, v: u16) {
        self.put(&v.to_be_bytes());
    }

    fn put_u32(&mut self, v: u32) {
        self.put(&v.to_be_bytes());
    }

    fn len(&self) -> usize {
        self.pos
    }
}

/// Bundles rendering resources so screen helpers don't need 6+ parameters.
struct UploadScreen<'a> {
    epd: &'a mut Epd,
    strip: &'a mut StripBuffer,
    delay: &'a mut Delay,
    heading: &'static BitmapFont,
    body: &'static BitmapFont,
    bumps: &'a ButtonFeedback,
}

impl<'a> UploadScreen<'a> {
    fn new(
        epd: &'a mut Epd,
        strip: &'a mut StripBuffer,
        delay: &'a mut Delay,
        ui_font_size_idx: u8,
        bumps: &'a ButtonFeedback,
    ) -> Self {
        Self {
            epd,
            strip,
            delay,
            heading: fonts::ui_heading_font(ui_font_size_idx),
            body: fonts::chrome_font(),
            bumps,
        }
    }

    /// Render lines with optional footer (partial refresh).
    async fn show(&mut self, lines: &[&str], footer: Option<&str>) {
        render_screen(
            self.epd, self.strip, self.delay,
            self.heading, self.body, lines, footer, self.bumps, false,
        ).await;
    }

    /// Render lines with optional footer (full refresh).
    async fn show_full(&mut self, lines: &[&str], footer: Option<&str>) {
        render_screen(
            self.epd, self.strip, self.delay,
            self.heading, self.body, lines, footer, self.bumps, true,
        ).await;
    }

    /// Show error message and wait for BACK button.
    async fn show_error(&mut self, msg: &str) {
        render_screen(
            self.epd, self.strip, self.delay,
            self.heading, self.body, &[msg], Some("Press BACK to exit"), self.bumps, false,
        ).await;
        drain_until_back().await;
    }
}

fn log_heap(label: &str) {
    let stats = esp_alloc::HEAP.stats();
    let free = stats.size - stats.current_usage;
    info!(
        "upload: heap [{}]: {}K used / {}K total ({}K free, {}K peak)",
        label,
        stats.current_usage / 1024,
        stats.size / 1024,
        free / 1024,
        stats.max_usage / 1024,
    );
}

type FileName = plump_kernel::util::FixedStr<13>;

enum ServerEvent {
    Nothing,
    Uploaded { name: FileName },
    UploadFailed,
    Deleted { name: FileName },
    DeleteFailed,
}

pub async fn run_upload_mode(
    wifi: esp_hal::peripherals::WIFI<'static>,
    epd: &mut Epd,
    strip: &mut StripBuffer,
    delay: &mut Delay,
    sd: &SdStorage,
    ui_font_size_idx: u8,
    bumps: &ButtonFeedback,
    wifi_cfg: &WifiConfig,
) {
    let mut screen = UploadScreen::new(epd, strip, delay, ui_font_size_idx, bumps);

    if !wifi_cfg.has_credentials() {
        screen.show(
            &[
                "No WiFi credentials!",
                "Set wifi_ssid in",
                // TODO: show actual data_dir name (_PLUMP or legacy _PULP)
                "_PLUMP/SETTINGS.TXT",
            ],
            Some("Press BACK to exit"),
        ).await;
        drain_until_back().await;
        return;
    }

    let ssid = wifi_cfg.ssid();
    let password = wifi_cfg.password();

    {
        let mut msg_buf = [0u8; 64];
        let msg_len = stack_fmt(&mut msg_buf, |w| {
            let _ = write!(w, "Connecting to '{}'...", ssid);
        });
        let msg = core::str::from_utf8(&msg_buf[..msg_len]).unwrap_or("Connecting...");
        screen.show_full(&[msg], None).await;
    }

    let radio = match esp_radio::init() {
        Ok(r) => r,
        Err(e) => {
            info!("upload: radio init failed: {:?}", e);
            screen.show_error("Radio init failed!").await;
            return;
        }
    };

    let (mut wifi_ctrl, interfaces) = match esp_radio::wifi::new(&radio, wifi, Config::default()) {
        Ok(pair) => pair,
        Err(e) => {
            info!("upload: wifi::new failed: {:?}", e);
            screen.show_error("WiFi init failed!").await;
            return;
        }
    };

    let client_cfg = ClientConfig::default()
        .with_ssid(String::from(ssid))
        .with_password(String::from(password));

    if let Err(e) = wifi_ctrl.set_config(&ModeConfig::Client(client_cfg)) {
        info!("upload: set_config failed: {:?}", e);
        screen.show_error("WiFi config error!").await;
        return;
    }

    if let Err(e) = wifi_ctrl.start_async().await {
        info!("upload: start failed: {:?}", e);
        screen.show_error("WiFi start failed!").await;
        return;
    }

    info!("upload: wifi started, connecting to '{}'", ssid);

    if let Err(e) = wifi_ctrl.connect_async().await {
        info!("upload: connect failed: {:?}", e);
        screen.show_error("Connection failed!").await;
        return;
    }

    info!("upload: connected to '{}'", ssid);

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let seed = {
        let rng = esp_hal::rng::Rng::new();
        (rng.random() as u64) << 32 | rng.random() as u64
    };

    let mut resources = embassy_net::StackResources::<4>::new();
    let (stack, mut runner) = embassy_net::new(interfaces.sta, net_config, &mut resources, seed);

    let got_ip = match select(
        runner.run(),
        select(stack.wait_config_up(), drain_until_back()),
    )
    .await
    {
        Either::Second(Either::First(_)) => true,
        Either::Second(Either::Second(_)) => false,
        _ => unreachable!(),
    };

    if !got_ip {
        info!("upload: user exited during DHCP");
        return;
    }

    let ip_octets: [u8; 4] = if let Some(cfg) = stack.config_v4() {
        cfg.address.address().octets()
    } else {
        [0, 0, 0, 0]
    };

    let mut ip_buf = [0u8; 48];
    let ip_len = stack_fmt(&mut ip_buf, |w| {
        let _ = write!(
            w,
            "({}.{}.{}.{})",
            ip_octets[0], ip_octets[1], ip_octets[2], ip_octets[3]
        );
    });
    let ip_str = core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("???");

    info!(
        "upload: serving at http://plump.local/  ({})",
        core::str::from_utf8(&ip_buf[1..ip_len.saturating_sub(1)]).unwrap_or("?")
    );
    log_heap("server ready");

    screen.show(
        &["http://plump.local/", ip_str],
        Some("Press BACK to exit"),
    ).await;

    let _ = stack.join_multicast_group(Ipv4Address::new(224, 0, 0, 251));

    let mut rx_buf = [0u8; TCP_RX_BUF_SIZE];
    let mut tx_buf = [0u8; TCP_TX_BUF_SIZE];

    // Persistent mDNS UDP socket — created once before the loop
    let mut mdns_rx_meta = [PacketMetadata::EMPTY; 2];
    let mut mdns_rx_buf = [0u8; 512];
    let mut mdns_tx_meta = [PacketMetadata::EMPTY; 2];
    let mut mdns_tx_buf = [0u8; 512];
    let mut mdns_socket = UdpSocket::new(
        stack,
        &mut mdns_rx_meta,
        &mut mdns_rx_buf,
        &mut mdns_tx_meta,
        &mut mdns_tx_buf,
    );
    let _ = mdns_socket.bind(MDNS_PORT);

    loop {
        match select(
            runner.run(),
            select3(
                serve_one_request(stack, &mut rx_buf, &mut tx_buf, sd),
                mdns_handle_one(&mut mdns_socket, ip_octets),
                drain_until_back(),
            ),
        )
        .await
        {
            Either::Second(Either3::First(event)) => {
                match event {
                    ServerEvent::Uploaded { name } => {
                        info!("upload: file saved as '{}'", name);
                    }
                    ServerEvent::UploadFailed => {
                        warn!("upload: file upload failed");
                    }
                    ServerEvent::Deleted { name } => {
                        info!("upload: deleted '{}'", name);
                    }
                    ServerEvent::DeleteFailed => {
                        warn!("upload: file delete failed");
                    }
                    ServerEvent::Nothing => {}
                }
            }
            Either::Second(Either3::Second(())) => continue,
            Either::Second(Either3::Third(())) => break,
            _ => unreachable!(),
        }
    }

    info!("upload: exiting, tearing down WiFi");
}

async fn serve_one_request(
    stack: embassy_net::Stack<'_>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    sd: &SdStorage,
) -> ServerEvent
where
{
    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);
    socket.set_timeout(Some(Duration::from_secs(HTTP_TIMEOUT_SECS)));

    if socket
        .accept(IpListenEndpoint {
            addr: None,
            port: 80,
        })
        .await
        .is_err()
    {
        Timer::after(Duration::from_millis(ACCEPT_RETRY_MS)).await;
        return ServerEvent::Nothing;
    }

    let mut hdr = [0u8; HTTP_HEADER_BUF_SIZE];
    let mut hdr_len = 0usize;

    loop {
        match socket.read(&mut hdr[hdr_len..]).await {
            Ok(0) => {
                close_socket(&mut socket).await;
                return ServerEvent::Nothing;
            }
            Ok(n) => {
                hdr_len += n;
                if hdr[..hdr_len].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if hdr_len >= hdr.len() {
                    let _ = socket
                        .write_all(b"HTTP/1.0 431 Headers Too Large\r\n\r\n")
                        .await;
                    close_socket(&mut socket).await;
                    return ServerEvent::Nothing;
                }
            }
            Err(_) => {
                close_socket(&mut socket).await;
                return ServerEvent::Nothing;
            }
        }
    }

    let headers_end = match find_subsequence(&hdr[..hdr_len], b"\r\n\r\n") {
        Some(p) => p,
        None => {
            close_socket(&mut socket).await;
            return ServerEvent::Nothing;
        }
    };
    let body_offset = headers_end + 4;
    let initial_body = &hdr[body_offset..hdr_len];
    let headers = &hdr[..headers_end];

    let first_line_end = headers
        .iter()
        .position(|&b| b == b'\r')
        .unwrap_or(headers.len());
    let request_line = &headers[..first_line_end];

    let is_get = request_line.starts_with(b"GET ");
    let is_post = request_line.starts_with(b"POST ");

    let path = extract_path(request_line);

    if is_get && path == b"/" {
        let _ = socket.write_all(HTTP_200_HTML).await;
        let _ = socket.write_all(UPLOAD_PAGE).await;
        let _ = socket.flush().await;
        close_socket(&mut socket).await;
        return ServerEvent::Nothing;
    }

    if is_get && path == b"/files" {
        let _ = socket.write_all(HTTP_200_JSON).await;

        let mut entries = [storage::DirEntry::EMPTY; DIR_LIST_MAX];
        let count = match sd.list_root_files(&mut entries) {
            Ok(n) => n,
            Err(_) => {
                let _ = socket.write_all(b"[]").await;
                let _ = socket.flush().await;
                close_socket(&mut socket).await;
                return ServerEvent::Nothing;
            }
        };

        let _ = socket.write_all(b"[").await;
        let mut json_buf = [0u8; 80]; // per-entry scratch: {"name":"XXXXXXXX.XXX","size":4294967295}
        for (i, e) in entries.iter().enumerate().take(count) {
            let name = e.name_str();
            let mut pos = 0usize;
            let prefix = b"{\"name\":\"";
            json_buf[..prefix.len()].copy_from_slice(prefix);
            pos += prefix.len();
            let nb = name.as_bytes();
            json_buf[pos..pos + nb.len()].copy_from_slice(nb);
            pos += nb.len();
            let mid = b"\",\"size\":";
            json_buf[pos..pos + mid.len()].copy_from_slice(mid);
            pos += mid.len();

            pos += fmt_u32(e.size, &mut json_buf[pos..]);
            json_buf[pos] = b'}';
            pos += 1;
            if i + 1 < count {
                json_buf[pos] = b',';
                pos += 1;
            }
            let _ = socket.write_all(&json_buf[..pos]).await;
        }
        let _ = socket.write_all(b"]").await;
        let _ = socket.flush().await;
        close_socket(&mut socket).await;
        return ServerEvent::Nothing;
    }

    if is_post && path == b"/upload" {
        let boundary = match find_boundary(headers) {
            Some(b) => b,
            None => {
                send_error_response(&mut socket, "Missing multipart boundary").await;
                close_socket(&mut socket).await;
                return ServerEvent::UploadFailed;
            }
        };

        match handle_upload(&mut socket, sd, boundary, initial_body).await {
            Ok(name) => {
                let _ = socket.write_all(HTTP_200_TEXT).await;
                let _ = socket.write_all(b"OK").await;
                let _ = socket.flush().await;
                close_socket(&mut socket).await;
                return ServerEvent::Uploaded { name };
            }
            Err(e) => {
                debug!("upload: handle_upload error: {}", e);
                send_error_response(&mut socket, e).await;
                close_socket(&mut socket).await;
                return ServerEvent::UploadFailed;
            }
        }
    }

    if is_post && path == b"/delete" {
        let content_len = extract_content_length(headers).unwrap_or(0);
        let max_body = content_len.min(13); // 8.3 filename max
        let mut body = [0u8; 16];
        let have = initial_body.len().min(body.len());
        body[..have].copy_from_slice(&initial_body[..have]);
        let mut body_len = have;

        while body_len < max_body && body_len < body.len() {
            match socket.read(&mut body[body_len..]).await {
                Ok(0) => break,
                Ok(n) => body_len += n,
                Err(_) => break,
            }
        }

        let name = match core::str::from_utf8(&body[..body_len]) {
            Ok(s) => s.trim(),
            Err(_) => {
                send_error_response(&mut socket, "Invalid filename").await;
                close_socket(&mut socket).await;
                return ServerEvent::DeleteFailed;
            }
        };

        if name.is_empty() || name.len() > 12 {
            send_error_response(&mut socket, "Invalid filename").await;
            close_socket(&mut socket).await;
            return ServerEvent::DeleteFailed;
        }

        match sd.delete_file(name) {
            Ok(()) => {
                let _ = socket.write_all(HTTP_200_TEXT).await;
                let _ = socket.write_all(b"OK").await;
                let _ = socket.flush().await;
                close_socket(&mut socket).await;
                return ServerEvent::Deleted {
                    name: FileName::from_bytes(name.as_bytes()),
                };
            }
            Err(e) => {
                debug!("upload: delete failed for '{}': {}", name, e);
                send_error_response(&mut socket, "delete failed").await;
                close_socket(&mut socket).await;
                return ServerEvent::DeleteFailed;
            }
        }
    }

    let _ = socket.write_all(HTTP_404).await;
    let _ = socket.flush().await;
    close_socket(&mut socket).await;
    ServerEvent::Nothing
}

async fn handle_upload(
    socket: &mut TcpSocket<'_>,
    sd: &SdStorage,
    boundary: &[u8],
    initial_body: &[u8],
) -> Result<FileName, &'static str>
where
{
    if boundary.len() > MAX_BOUNDARY_LEN {
        return Err("boundary too long");
    }

    let em_len = 4 + boundary.len();
    let mut end_marker_buf = [0u8; MAX_BOUNDARY_LEN + 4];
    end_marker_buf[0] = b'\r';
    end_marker_buf[1] = b'\n';
    end_marker_buf[2] = b'-';
    end_marker_buf[3] = b'-';
    end_marker_buf[4..em_len].copy_from_slice(boundary);
    let end_marker = &end_marker_buf[..em_len];

    let mut work = [0u8; WORK_BUF_SIZE];
    let init_len = initial_body.len().min(work.len());
    work[..init_len].copy_from_slice(&initial_body[..init_len]);
    let mut filled = init_len;

    let file_name = loop {
        if let Some(pos) = find_subsequence(&work[..filled], b"\r\n\r\n") {
            let part_headers = &work[..pos];

            let raw_name = extract_filename(part_headers).ok_or("no filename in upload")?;
            let name = sanitize_83(raw_name);
            if name.is_empty() {
                return Err("invalid filename");
            }

            // warn if sanitisation changed the name, two different
            // original names can map to the same 8.3 name, causing
            // the second upload to silently overwrite the first.
            if raw_name != name.as_bytes() {
                log::warn!(
                    "upload: sanitised '{}' -> '{}' (may overwrite existing file)",
                    core::str::from_utf8(raw_name).unwrap_or("?"),
                    name,
                );
            }

            let file_start = pos + 4;
            work.copy_within(file_start..filled, 0);
            filled -= file_start;

            break name;
        }

        if filled >= work.len() {
            return Err("part headers too large");
        }

        let n = socket
            .read(&mut work[filled..])
            .await
            .map_err(|_| "read error")?;
        if n == 0 {
            return Err("connection closed during headers");
        }
        filled += n;
    };

    let name_str = file_name.as_str();

    debug!("upload: receiving file '{}'", name_str);
    log_heap("upload start");

    // open file once for the entire upload (create/truncate)
    let file = sd.create_file(name_str).map_err(|_| "create failed")?;

    // holdback last end_marker.len() bytes to detect boundary spanning two reads

    let mut total_written: u32 = 0;

    let result = loop {
        if let Some(pos) = find_subsequence(&work[..filled], end_marker) {
            if pos > 0 {
                if let Err(_) = file.write(sd, &work[..pos]) {
                    break Err("write failed");
                }
                total_written += pos as u32;
            }
            debug!("upload: complete, {} bytes written", total_written);
            break Ok(());
        }

        if filled > end_marker.len() {
            let safe = filled - end_marker.len();
            if let Err(_) = file.write(sd, &work[..safe]) {
                break Err("write failed");
            }
            total_written += safe as u32;

            work.copy_within(safe..filled, 0);
            filled = end_marker.len();
        }

        let n = socket
            .read(&mut work[filled..])
            .await
            .map_err(|_| "read error during upload")?;
        if n == 0 {
            if filled > 0 {
                let _ = file.write(sd, &work[..filled]);
            }
            // close before returning error so the handle is not leaked
            let _ = file.close(sd);
            return Err("upload incomplete");
        }
        filled += n;
    };

    // always close — whether write loop succeeded or failed
    let _ = file.close(sd);
    log_heap("upload done");
    result?;
    Ok(file_name)
}

fn extract_path(line: &[u8]) -> &[u8] {
    let start = match line.iter().position(|&b| b == b' ') {
        Some(p) => p + 1,
        None => return b"/",
    };

    let rest = &line[start..];
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());

    let path = &rest[..end];
    let qmark = path.iter().position(|&b| b == b'?').unwrap_or(path.len());
    &path[..qmark]
}

fn find_boundary(headers: &[u8]) -> Option<&[u8]> {
    let marker = b"boundary=";
    let pos = headers
        .windows(marker.len())
        .position(|w| w.eq_ignore_ascii_case(marker))?;
    let start = pos + marker.len();
    let rest = &headers[start..];

    if rest.is_empty() {
        return None;
    }

    if rest[0] == b'"' {
        let inner = &rest[1..];
        let end = inner.iter().position(|&b| b == b'"')?;
        if end == 0 {
            return None;
        }
        Some(&inner[..end])
    } else {
        let end = rest
            .iter()
            .position(|&b| b == b'\r' || b == b'\n' || b == b';' || b == b' ')
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        Some(&rest[..end])
    }
}

fn extract_filename(headers: &[u8]) -> Option<&[u8]> {
    let marker = b"filename=\"";
    let pos = headers
        .windows(marker.len())
        .position(|w| w.eq_ignore_ascii_case(marker))?;
    let start = pos + marker.len();
    let rest = &headers[start..];
    let end = rest.iter().position(|&b| b == b'"')?;
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

fn sanitize_83(raw: &[u8]) -> FileName {
    let name = match raw.iter().rposition(|&b| b == b'/' || b == b'\\') {
        Some(p) => &raw[p + 1..],
        None => raw,
    };

    let (base_src, ext_src) = match name.iter().rposition(|&b| b == b'.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, &[] as &[u8]),
    };

    let mut out = [0u8; 13];
    let mut pos: usize = 0;

    for &b in base_src.iter() {
        if pos >= 8 {
            break;
        }
        if is_valid_83_char(b) {
            out[pos] = b.to_ascii_uppercase();
            pos += 1;
        }
    }

    if pos == 0 {
        out[..6].copy_from_slice(b"UPLOAD");
        pos = 6;
    }

    if !ext_src.is_empty() {
        out[pos] = b'.';
        pos += 1;
        let ext_start = pos;
        for &b in ext_src.iter() {
            if pos - ext_start >= 3 {
                break;
            }
            if is_valid_83_char(b) {
                out[pos] = b.to_ascii_uppercase();
                pos += 1;
            }
        }

        if pos == ext_start {
            pos -= 1;
        }
    }

    FileName::from_raw(out, pos as u8)
}

fn is_valid_83_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'~' | b'!' | b'#' | b'$' | b'&')
}

async fn send_error_response(socket: &mut TcpSocket<'_>, msg: &str) {
    let _ = socket.write_all(HTTP_500_TEXT).await;
    let _ = socket.write_all(msg.as_bytes()).await;
    let _ = socket.flush().await;
}

fn fmt_u32(mut n: u32, buf: &mut [u8]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut pos = 0;
    while n > 0 {
        tmp[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        pos += 1;
    }
    for i in 0..pos {
        buf[i] = tmp[pos - 1 - i];
    }
    pos
}

fn extract_content_length(headers: &[u8]) -> Option<usize> {
    let marker = b"content-length:";
    let pos = headers
        .windows(marker.len())
        .position(|w| w.eq_ignore_ascii_case(marker))?;
    let start = pos + marker.len();
    let rest = &headers[start..];

    let trimmed = rest.iter().position(|&b| b != b' ' && b != b'\t')?;
    let rest = &rest[trimmed..];
    let end = rest
        .iter()
        .position(|&b| b == b'\r' || b == b'\n')
        .unwrap_or(rest.len());
    let digits = &rest[..end];
    let mut val: usize = 0;
    for &b in digits {
        if b.is_ascii_digit() {
            val = val.saturating_mul(10).saturating_add((b - b'0') as usize);
        } else {
            break;
        }
    }
    Some(val)
}

async fn close_socket(socket: &mut TcpSocket<'_>) {
    socket.close();
    Timer::after(Duration::from_millis(SOCKET_CLOSE_DELAY_MS)).await;
    socket.abort();
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn mdns_handle_one(socket: &mut UdpSocket<'_>, ip_octets: [u8; 4]) {
    let mut pkt = [0u8; 256];
    let (n, _remote) = match socket.recv_from(&mut pkt).await {
        Ok(r) => r,
        Err(_) => return,
    };

    if !is_mdns_query_for_plump(&pkt[..n]) {
        return;
    }

    debug!("upload: mDNS query for plump.local -- responding");

    let mut resp = [0u8; 64];
    let len = build_mdns_response(&mut resp, ip_octets);

    let mdns_dest = embassy_net::IpEndpoint::new(
        embassy_net::IpAddress::Ipv4(Ipv4Address::from(MDNS_MULTICAST)),
        MDNS_PORT,
    );
    let _ = socket.send_to(&resp[..len], mdns_dest).await;
}

fn is_mdns_query_for_plump(pkt: &[u8]) -> bool {
    // DNS header (12) + qname + qtype (2) + qclass (2)
    let min_len = 12 + HOSTNAME_WIRE.len() + 4;
    if pkt.len() < min_len {
        return false;
    }

    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    if flags & 0x8000 != 0 {
        return false; // response, not query
    }

    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]);
    if qdcount < 1 {
        return false;
    }

    // compare qname against HOSTNAME_WIRE (case-insensitive for labels)
    let qname_end = 12 + HOSTNAME_WIRE.len();
    let qname = &pkt[12..qname_end];

    // verify label structure: first label length, second label length, NUL
    if qname[0] != HOSTNAME_WIRE[0] {
        return false;
    }
    let label1_len = qname[0] as usize;
    if qname[1 + label1_len] != HOSTNAME_WIRE[1 + label1_len] {
        return false;
    }
    let label2_len = qname[1 + label1_len] as usize;
    if qname[1 + label1_len + 1 + label2_len] != 0 {
        return false;
    }

    // case-insensitive label comparison
    if !qname[1..1 + label1_len].eq_ignore_ascii_case(&HOSTNAME_WIRE[1..1 + label1_len]) {
        return false;
    }
    let l2_start = 1 + label1_len + 1;
    if !qname[l2_start..l2_start + label2_len]
        .eq_ignore_ascii_case(&HOSTNAME_WIRE[l2_start..l2_start + label2_len])
    {
        return false;
    }

    let qtype = u16::from_be_bytes([pkt[qname_end], pkt[qname_end + 1]]);
    let qclass = u16::from_be_bytes([pkt[qname_end + 2], pkt[qname_end + 3]]) & 0x7FFF;

    (qtype == 1 || qtype == 255) && qclass == 1
}

fn build_mdns_response(buf: &mut [u8], ip: [u8; 4]) -> usize {
    let mut w = DnsBuf::new(buf);
    w.put_u16(0x0000);       // transaction ID
    w.put_u16(0x8400);       // flags: response, authoritative
    w.put_u16(0x0000);       // QDCOUNT
    w.put_u16(0x0001);       // ANCOUNT
    w.put_u16(0x0000);       // NSCOUNT
    w.put_u16(0x0000);       // ARCOUNT
    w.put(&HOSTNAME_WIRE);   // name
    w.put_u16(0x0001);       // TYPE A
    w.put_u16(0x8001);       // CLASS IN, cache-flush
    w.put_u32(120);          // TTL 120s
    w.put_u16(0x0004);       // RDLENGTH
    w.put(&ip);              // RDATA (IPv4 address)
    w.len()
}

async fn drain_until_back() {
    let mapper = ButtonMapper::new();
    loop {
        let hw = tasks::INPUT_EVENTS.receive().await;
        let ev = mapper.map_event(hw);
        if matches!(
            ev,
            ActionEvent::Press(Action::Back) | ActionEvent::LongPress(Action::Back)
        ) {
            return;
        }
    }
}

async fn render_screen(
    epd: &mut Epd,
    strip: &mut StripBuffer,
    delay: &mut Delay,
    heading: &'static BitmapFont,
    body: &'static BitmapFont,
    lines: &[&str],
    footer: Option<&str>,
    bumps: &ButtonFeedback,
    full_refresh: bool,
) {
    let heading_h = heading.line_height;
    let body_h = body.line_height;
    let body_stride = body_h + BODY_LINE_GAP;

    let heading_region = Region::new(HEADING_X, CONTENT_TOP + 12, HEADING_W, heading_h);

    let body_area_top = CONTENT_TOP + 12 + heading_h + 40;
    let body_area_bottom = FOOTER_Y.saturating_sub(20);
    let body_area_h = body_area_bottom.saturating_sub(body_area_top);
    let total_body_h = if lines.is_empty() {
        0
    } else {
        (lines.len() as u16 - 1) * body_stride + body_h
    };
    let body_start_y = body_area_top + body_area_h.saturating_sub(total_body_h) / 2;

    let footer_region = Region::new(BODY_X, FOOTER_Y, BODY_W, body_h);

    let draw = |s: &mut StripBuffer| {
        use embedded_graphics::pixelcolor::BinaryColor;
        use embedded_graphics::prelude::*;
        use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};

        BitmapLabel::new(heading_region, "Upload", heading)
            .alignment(Alignment::CenterLeft)
            .draw(s)
            .unwrap();

        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let y = body_start_y + (i as u16) * body_stride;
            let region = Region::new(BODY_X, y, BODY_W, body_h);
            BitmapLabel::new(region, line, body)
                .alignment(Alignment::Center)
                .draw(s)
                .unwrap();
        }

        // QR placeholder square below the connection info. real QR
        // encoder is a follow-up; for now we draw a labelled box so
        // the layout matches the mockup intent (mockup also shows the
        // QR centred below the host name).
        let qr_size: u16 = 160;
        let qr_x = (SCREEN_W - qr_size) / 2;
        let qr_y = body_start_y + (lines.len() as u16) * body_stride + 12;
        if qr_y + qr_size < FOOTER_Y.saturating_sub(8) {
            let qr_rect = Rectangle::new(
                Point::new(qr_x as i32, qr_y as i32),
                Size::new(qr_size as u32, qr_size as u32),
            );
            qr_rect
                .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 2))
                .draw(s)
                .ok();
            let qr_label = Region::new(qr_x, qr_y + qr_size / 2 - body_h / 2, qr_size, body_h);
            BitmapLabel::new(qr_label, "QR coming", body)
                .alignment(Alignment::Center)
                .draw(s)
                .unwrap();
        }

        if let Some(text) = footer {
            BitmapLabel::new(footer_region, text, body)
                .alignment(Alignment::Center)
                .draw(s)
                .unwrap();
        }

        bumps.draw(s);
    };

    let result = if full_refresh {
        epd.full_refresh_async(strip, delay, &draw).await
    } else {
        epd.partial_refresh_async(strip, delay, 0, 0, SCREEN_W, SCREEN_H, &draw)
            .await
    };
    if result.is_err() {
        log::warn!("upload: EPD refresh timed out");
    }
}
