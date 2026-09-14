// wifi upload server: HTTP file upload + mDNS (plump.local)

mod dhcp;

use alloc::string::String;
use core::fmt::Write as FmtWrite;
use core::future::pending;

use embassy_futures::select::{Either, Either3, Either4, select, select3, select4};
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpListenEndpoint, Ipv4Address, Ipv4Cidr, StaticConfigV4};
use embassy_time::{Duration, Instant, Timer, with_deadline};
use embedded_io_async::Write as AsyncWrite;
use esp_radio::wifi::{
    AccessPointConfig, AuthMethod, ClientConfig, Config, ModeConfig, WifiDevice, WifiEvent,
};
use log::{debug, info, warn};

use crate::apps::Tab;
use crate::board::action::{Action, ActionEvent, ButtonMapper};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::fonts::bitmap::BitmapFont;
use crate::kernel::Screen;
use crate::kernel::config::WifiConfig;
use crate::kernel::tasks;
use crate::ui::chrome::Chrome;
use crate::ui::{
    Alignment, BitmapLabel, CONTENT_TOP, LARGE_MARGIN, Painter, QrSymbol, Region, Theme, stack_fmt,
};

const HEADING_X: u16 = LARGE_MARGIN;
const HEADING_W: u16 = SCREEN_W - HEADING_X * 2;

const BODY_X: u16 = 24;
const BODY_W: u16 = SCREEN_W - BODY_X * 2;
const BODY_LINE_GAP: u16 = 10;
/// gap between the caption block and the QR below it
const QR_GAP: u16 = 16;

// the footer sits in the band the tab bar leaves, not at a hardcoded
// offset from the panel edge
const TAB_BAR_TOP: u16 = Theme::default_v1().content_bottom();
const FOOTER_H: u16 = 28;
const FOOTER_Y: u16 = TAB_BAR_TOP - FOOTER_H;

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

const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const QTYPE_NSEC: u16 = 47;
const QTYPE_ANY: u16 = 255;
const QCLASS_IN: u16 = 1;
/// top bit of a question's class: "answer me directly"
const MDNS_UNICAST_BIT: u16 = 0x8000;
/// class IN with the cache-flush bit, for records we are authoritative for
const MDNS_CLASS_FLUSH: u16 = 0x8001;
const MDNS_TTL_SECS: u32 = 120;
const MDNS_ANNOUNCEMENTS: u8 = 3;
const MDNS_ANNOUNCE_GAP: Duration = Duration::from_secs(1);
/// name + NSEC rdata (name again + bitmap) plus the 12-byte header
const MDNS_RESP_MAX: usize = 64;

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

const STATION_DEADLINE_SECS: u64 = 8;
const FALLBACK_SSID: &str = "PLUMP-X4";
const FALLBACK_PASSWORD: &str = "plumpbooks";
const FALLBACK_IP: [u8; 4] = dhcp::SERVER_IP;

/// Why the upload screen stopped, and where the user expects to land.
///
/// Upload owns the input channel for its whole run, so every way out
/// of it is decided here. `Back` and `Tab` differ only in destination,
/// which is exactly what the `exit_requested` bool could not carry:
/// it recorded that the user left, never where to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadExit {
    /// Back: return to the tab upload was opened from.
    Back,
    /// Edge navigation: the user paged into a neighbouring tab.
    Tab(Tab),
}

/// How the device is reachable.
///
/// The screen text, the QR payload and the mDNS record all want the
/// same fact; deriving it once means they cannot disagree about which
/// network the user is being pointed at.
#[derive(Clone, Copy)]
enum Endpoint {
    /// Joined the configured network.
    Station { ip: [u8; 4] },
    /// Hosting the fallback AP. A client with no lease cannot reach
    /// the URL at all, so `joined` decides whether the screen offers
    /// the join credential or the address.
    SoftAp { ip: [u8; 4], joined: bool },
}

/// Fits `WIFI:T:WPA;S:PLUMP-X4;P:plumpbooks;;` and any dotted-quad URL.
const QR_PAYLOAD_MAX: usize = 64;

impl Endpoint {
    #[inline]
    const fn ip(&self) -> [u8; 4] {
        match self {
            Self::Station { ip } | Self::SoftAp { ip, .. } => *ip,
        }
    }

    /// What the QR encodes: a join credential while the client is not
    /// on the network yet, the address once it is.
    ///
    /// Phone cameras join a network straight from a `WIFI:` payload,
    /// which saves typing a password that only exists to keep the
    /// upload window closed to the rest of the street.
    fn qr_payload(&self, buf: &mut [u8; QR_PAYLOAD_MAX]) -> usize {
        let ip = self.ip();
        match self {
            Self::SoftAp { joined: false, .. } => stack_fmt(buf, |w| {
                let _ = write!(
                    w,
                    "WIFI:T:WPA;S:{};P:{};;",
                    FALLBACK_SSID, FALLBACK_PASSWORD
                );
            }),
            _ => stack_fmt(buf, |w| {
                let _ = write!(w, "http://{}.{}.{}.{}/", ip[0], ip[1], ip[2], ip[3]);
            }),
        }
    }
}

/// How far the configured-network attempt got.
///
/// One value for one outcome: the old `station_started` / `connected`
/// / `got_ip` / `exit_requested` bools had 16 combinations, of which
/// three were reachable.
enum Station {
    /// A session ran on the configured network and the user left.
    Served(UploadExit),
    /// Never became usable; host the fallback AP instead.
    Failed,
}

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
    screen: &'a mut Screen,
    heading: &'static BitmapFont,
    body: &'static BitmapFont,
    /// the same chrome the dispatch loop draws. upload runs outside
    /// that loop, so it paints the bars itself rather than dropping
    /// them for the length of the session
    chrome: &'a Chrome,
    mapper: &'a ButtonMapper,
}

impl<'a> UploadScreen<'a> {
    fn new(
        screen: &'a mut Screen,
        ui_font_size_idx: u8,
        chrome: &'a Chrome,
        mapper: &'a ButtonMapper,
    ) -> Self {
        Self {
            screen,
            heading: fonts::ui_heading_font(ui_font_size_idx),
            body: fonts::chrome_font(),
            chrome,
            mapper,
        }
    }

    /// Paint the ready screen for an endpoint: the instructions and
    /// the QR that matches them, from one description of where the
    /// device is.
    async fn show_endpoint(&mut self, endpoint: &Endpoint, full_refresh: bool) {
        let mut payload = [0u8; QR_PAYLOAD_MAX];
        let payload_len = endpoint.qr_payload(&mut payload);
        let payload_str = core::str::from_utf8(&payload[..payload_len]).unwrap_or("");
        let qr = QrSymbol::encode(payload_str);
        if qr.is_none() {
            warn!("upload: no QR for {} byte payload", payload_len);
        }

        let ip = endpoint.ip();
        let mut ip_buf = [0u8; 32];
        let ip_len = stack_fmt(&mut ip_buf, |w| {
            let _ = write!(w, "({}.{}.{}.{})", ip[0], ip[1], ip[2], ip[3]);
        });
        let ip_str = core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("");

        let mut join_buf = [0u8; 64];
        let join_len = stack_fmt(&mut join_buf, |w| {
            let _ = write!(w, "{} / {}", FALLBACK_SSID, FALLBACK_PASSWORD);
        });
        let join_str = core::str::from_utf8(&join_buf[..join_len]).unwrap_or("");

        let mut lines = [""; 2];
        let count = match endpoint {
            Endpoint::SoftAp { joined: false, .. } => {
                lines[0] = "Scan to join, or connect to";
                lines[1] = join_str;
                2
            }
            _ => {
                lines[0] = "http://plump.local/";
                lines[1] = ip_str;
                2
            }
        };

        self.render(
            &lines[..count],
            Some("Press BACK to exit"),
            qr.as_ref(),
            full_refresh,
        )
        .await;
    }

    /// Render lines with optional footer (full refresh).
    async fn show_full(&mut self, lines: &[&str], footer: Option<&str>) {
        self.render(lines, footer, None, true).await;
    }

    /// Show an error and wait for the user to leave.
    async fn show_error(&mut self, msg: &str) -> UploadExit {
        self.render(&[msg], Some("Press BACK to exit"), None, false)
            .await;
        wait_for_exit(self.mapper).await
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

/// Removes the partial file when an upload does not run to completion.
///
/// The FAT handle itself is the [`storage::FileWriter`]'s business: it
/// closes on drop, so this guard only owns the commit decision.
struct UploadFileGuard<'a> {
    file: Option<storage::FileWriter<'a>>,
    sd: &'a SdStorage,
    name: FileName,
    committed: bool,
}

impl<'a> UploadFileGuard<'a> {
    fn new(file: storage::FileWriter<'a>, sd: &'a SdStorage, name: FileName) -> Self {
        Self {
            file: Some(file),
            sd,
            name,
            committed: false,
        }
    }

    fn write(&self, data: &[u8]) -> crate::error::Result<()> {
        self.file
            .as_ref()
            .expect("upload file already closed")
            .write(data)
    }

    fn finish(mut self) -> crate::error::Result<()> {
        let result = self
            .file
            .take()
            .expect("upload file already closed")
            .close();
        if result.is_ok() {
            self.committed = true;
        }
        result
    }
}

impl Drop for UploadFileGuard<'_> {
    fn drop(&mut self) {
        // close before unlinking: never delete an entry the volume
        // manager still holds open
        drop(self.file.take());
        if !self.committed {
            if let Err(e) = self.sd.delete_file(self.name.as_str()) {
                warn!("upload: failed to remove incomplete '{}': {}", self.name, e);
            } else {
                info!("upload: removed incomplete '{}'", self.name);
            }
        }
    }
}

pub async fn run_upload_mode(
    wifi: esp_hal::peripherals::WIFI<'static>,
    screen: &mut Screen,
    sd: &SdStorage,
    ui_font_size_idx: u8,
    chrome: &Chrome,
    mapper: &ButtonMapper,
    wifi_cfg: &WifiConfig,
) -> UploadExit {
    let mut screen = UploadScreen::new(screen, ui_font_size_idx, chrome, mapper);

    let radio = match esp_radio::init() {
        Ok(r) => r,
        Err(e) => {
            info!("upload: radio init failed: {:?}", e);
            return screen.show_error("Radio init failed!").await;
        }
    };

    let (mut wifi_ctrl, interfaces) = match esp_radio::wifi::new(&radio, wifi, Config::default()) {
        Ok(pair) => pair,
        Err(e) => {
            info!("upload: wifi::new failed: {:?}", e);
            return screen.show_error("WiFi init failed!").await;
        }
    };

    let seed = {
        let rng = esp_hal::rng::Rng::new();
        (rng.random() as u64) << 32 | rng.random() as u64
    };

    let mut station_started = false;

    let station = 'station: {
        if !wifi_cfg.has_credentials() {
            info!("upload: no configured WiFi, starting fallback AP");
            break 'station Station::Failed;
        }

        let ssid = wifi_cfg.ssid();
        let mut msg_buf = [0u8; 64];
        let msg_len = stack_fmt(&mut msg_buf, |w| {
            let _ = write!(w, "Connecting to '{}'...", ssid);
        });
        let message = core::str::from_utf8(&msg_buf[..msg_len]).unwrap_or("Connecting...");
        screen.show_full(&[message], Some("BACK cancels")).await;

        let client_cfg = ClientConfig::default()
            .with_ssid(String::from(ssid))
            .with_password(String::from(wifi_cfg.password()));

        match wifi_ctrl.set_config(&ModeConfig::Client(client_cfg)) {
            Ok(()) => match wifi_ctrl.start_async().await {
                Ok(()) => station_started = true,
                Err(e) => warn!("upload: station start failed, falling back: {:?}", e),
            },
            Err(e) => warn!("upload: station config failed, falling back: {:?}", e),
        }

        if !station_started {
            break 'station Station::Failed;
        }

        let deadline = Instant::now() + Duration::from_secs(STATION_DEADLINE_SECS);
        info!(
            "upload: trying configured WiFi '{}' for {}s",
            ssid, STATION_DEADLINE_SECS
        );
        match select(
            with_deadline(deadline, wifi_ctrl.connect_async()),
            wait_for_exit(mapper),
        )
        .await
        {
            Either::First(Ok(Ok(()))) => {}
            Either::First(Ok(Err(e))) => {
                warn!("upload: station association failed, falling back: {:?}", e);
                break 'station Station::Failed;
            }
            Either::First(Err(_)) => {
                warn!("upload: station association timed out, falling back");
                break 'station Station::Failed;
            }
            Either::Second(exit) => break 'station Station::Served(exit),
        }

        let mut resources = embassy_net::StackResources::<4>::new();
        let net_config = embassy_net::Config::dhcpv4(Default::default());
        let (stack, mut runner) =
            embassy_net::new(interfaces.sta, net_config, &mut resources, seed);

        match select3(
            runner.run(),
            with_deadline(deadline, stack.wait_config_up()),
            wait_for_exit(mapper),
        )
        .await
        {
            Either3::First(never) => match never {},
            Either3::Second(Ok(())) => {}
            Either3::Second(Err(_)) => {
                warn!("upload: station DHCP timed out, falling back");
                break 'station Station::Failed;
            }
            Either3::Third(exit) => break 'station Station::Served(exit),
        }

        let ip = stack
            .config_v4()
            .map(|cfg| cfg.address.address().octets())
            .unwrap_or([0, 0, 0, 0]);
        let mut endpoint = Endpoint::Station { ip };
        show_server_ready(&mut screen, &endpoint).await;
        info!("upload: connected to '{}'", ssid);

        match select(
            wifi_ctrl.wait_for_event(WifiEvent::StaDisconnected),
            serve_network(
                stack,
                &mut runner,
                sd,
                &mut endpoint,
                None,
                mapper,
                &mut screen,
            ),
        )
        .await
        {
            Either::First(()) => {
                warn!("upload: configured WiFi disconnected, falling back");
                Station::Failed
            }
            Either::Second(exit) => Station::Served(exit),
        }
    };

    if station_started {
        let _ = wifi_ctrl.stop_async().await;
    }
    if let Station::Served(exit) = station {
        info!("upload: leaving station session: {:?}", exit);
        return exit;
    }

    let ap_config = AccessPointConfig::default()
        .with_ssid(String::from(FALLBACK_SSID))
        .with_auth_method(AuthMethod::Wpa2Personal)
        .with_password(String::from(FALLBACK_PASSWORD))
        .with_max_connections(1);
    if let Err(e) = wifi_ctrl.set_config(&ModeConfig::AccessPoint(ap_config)) {
        warn!("upload: fallback AP config failed: {:?}", e);
        return screen.show_error("Fallback WiFi failed!").await;
    }
    if let Err(e) = wifi_ctrl.start_async().await {
        warn!("upload: fallback AP start failed: {:?}", e);
        return screen.show_error("Fallback WiFi failed!").await;
    }

    let net_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(
            Ipv4Address::new(
                FALLBACK_IP[0],
                FALLBACK_IP[1],
                FALLBACK_IP[2],
                FALLBACK_IP[3],
            ),
            24,
        ),
        gateway: None,
        dns_servers: Default::default(),
    });
    let mut resources = embassy_net::StackResources::<4>::new();
    let (stack, mut runner) = embassy_net::new(interfaces.ap, net_config, &mut resources, seed);

    let mut dhcp_rx_meta = [PacketMetadata::EMPTY; 2];
    let mut dhcp_rx_buf = [0u8; 1200];
    let mut dhcp_tx_meta = [PacketMetadata::EMPTY; 2];
    let mut dhcp_tx_buf = [0u8; 1200];
    let mut dhcp_socket = UdpSocket::new(
        stack,
        &mut dhcp_rx_meta,
        &mut dhcp_rx_buf,
        &mut dhcp_tx_meta,
        &mut dhcp_tx_buf,
    );
    if !dhcp::bind(&mut dhcp_socket) {
        warn!("upload: DHCP server bind failed");
        let exit = screen.show_error("Fallback DHCP failed!").await;
        let _ = wifi_ctrl.stop_async().await;
        return exit;
    }

    info!(
        "upload: fallback AP '{}' ready at 192.168.4.1",
        FALLBACK_SSID
    );
    let mut endpoint = Endpoint::SoftAp {
        ip: FALLBACK_IP,
        joined: false,
    };
    screen.show_endpoint(&endpoint, true).await;
    let exit = serve_network(
        stack,
        &mut runner,
        sd,
        &mut endpoint,
        Some(&mut dhcp_socket),
        mapper,
        &mut screen,
    )
    .await;
    let _ = wifi_ctrl.stop_async().await;
    info!("upload: exiting, WiFi stopped");
    exit
}

async fn show_server_ready(screen: &mut UploadScreen<'_>, endpoint: &Endpoint) {
    let ip = endpoint.ip();
    info!(
        "upload: serving at http://plump.local/ ({}.{}.{}.{})",
        ip[0], ip[1], ip[2], ip[3]
    );
    log_heap("server ready");
    screen.show_endpoint(endpoint, false).await;
}

async fn serve_network<'stack, 'device>(
    stack: embassy_net::Stack<'stack>,
    runner: &mut embassy_net::Runner<'stack, WifiDevice<'device>>,
    sd: &SdStorage,
    endpoint: &mut Endpoint,
    mut dhcp_socket: Option<&mut UdpSocket<'_>>,
    mapper: &ButtonMapper,
    screen: &mut UploadScreen<'_>,
) -> UploadExit {
    let ip = endpoint.ip();
    if let Err(e) = stack.join_multicast_group(Ipv4Address::from(MDNS_MULTICAST)) {
        // without the group the IP layer drops every query before the
        // socket sees it, and the responder looks like it is running
        warn!("upload: mDNS multicast join failed: {:?}", e);
    }

    let mut rx_buf = [0u8; TCP_RX_BUF_SIZE];
    let mut tx_buf = [0u8; TCP_TX_BUF_SIZE];
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
    if let Err(e) = mdns_socket.bind(MDNS_PORT) {
        warn!("upload: mDNS bind failed: {:?}", e);
    }

    // the HTTP server is one future for the whole session, pinned here
    // and re-polled through the select rather than rebuilt by it. built
    // inside the select, it was dropped every time a sibling branch
    // completed, taking the TcpSocket with it: an mDNS packet arriving
    // during a browser's handshake reset the connection. pinning it
    // costs nothing on top of what the select already reserved, whereas
    // wrapping the branches in long-lived async blocks cost 13K of task
    // future, which this device does not have
    let mut http = core::pin::pin!(serve_http(stack, &mut rx_buf, &mut tx_buf, sd));

    let mut announcer = Announcer::new();
    loop {
        match select(
            runner.run(),
            select4(
                http.as_mut(),
                mdns_step(&mut mdns_socket, ip, &mut announcer),
                dhcp_handle_one(&mut dhcp_socket),
                wait_for_exit(mapper),
            ),
        )
        .await
        {
            Either::First(never) => match never {},
            Either::Second(Either4::First(())) => unreachable!("serve_http never returns"),
            Either::Second(Either4::Second(())) => {}
            Either::Second(Either4::Third(served)) => {
                // the lease is the first moment the client can reach
                // the server, so the join credential stops being the
                // useful thing to show
                if served == dhcp::Served::Bound
                    && let Endpoint::SoftAp { joined, .. } = endpoint
                    && !*joined
                {
                    *joined = true;
                    info!("upload: client bound, showing the address");
                    screen.show_endpoint(endpoint, false).await;
                }
            }
            Either::Second(Either4::Fourth(exit)) => return exit,
        }
    }
}

/// Serve HTTP requests until the session ends.
///
/// Never returns: the caller's exit arm is what ends the session, and
/// a request that fails is logged and followed by the next accept.
async fn serve_http(
    stack: embassy_net::Stack<'_>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    sd: &SdStorage,
) {
    loop {
        match serve_one_request(stack, rx_buf, tx_buf, sd).await {
            ServerEvent::Uploaded { name } => info!("upload: file saved as '{}'", name),
            ServerEvent::UploadFailed => warn!("upload: file upload failed"),
            ServerEvent::Deleted { name } => info!("upload: deleted '{}'", name),
            ServerEvent::DeleteFailed => warn!("upload: file delete failed"),
            ServerEvent::Nothing => {}
        }
    }
}

async fn dhcp_handle_one(socket: &mut Option<&mut UdpSocket<'_>>) -> dhcp::Served {
    if let Some(socket) = socket.as_deref_mut() {
        dhcp::handle_one(socket).await
    } else {
        pending::<dhcp::Served>().await
    }
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
    let file = UploadFileGuard::new(file, sd, file_name);

    // holdback last end_marker.len() bytes to detect boundary spanning two reads

    let mut total_written: u32 = 0;

    let result = loop {
        if let Some(pos) = find_subsequence(&work[..filled], end_marker) {
            if pos > 0 {
                if file.write(&work[..pos]).is_err() {
                    break Err("write failed");
                }
                total_written += pos as u32;
            }
            debug!("upload: complete, {} bytes written", total_written);
            break Ok(());
        }

        if filled > end_marker.len() {
            let safe = filled - end_marker.len();
            if file.write(&work[..safe]).is_err() {
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
                let _ = file.write(&work[..filled]);
            }
            return Err("upload incomplete");
        }
        filled += n;
    };

    result?;
    file.finish().map_err(|_| "close failed")?;
    log_heap("upload done");
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

/// Answer one query, or send the announcement that has fallen due.
///
/// The query is decided inside the socket's own buffer: a query is
/// read once and never needed again, and the second copy would be
/// half a kilobyte of task future on a device that counts it.
async fn mdns_step(socket: &mut UdpSocket<'_>, ip: [u8; 4], announcer: &mut Announcer) {
    let decide =
        |pkt: &[u8], meta: embassy_net::udp::UdpMetadata| (Answer::decide(pkt), meta.endpoint);

    let (answer, from) = match announcer.deadline() {
        Some(at) => match select(socket.recv_from_with(decide), Timer::at(at)).await {
            Either::First(decided) => decided,
            Either::Second(()) => {
                announcer.fired();
                send_answer(socket, Answer::Address, ip, Destination::Multicast).await;
                debug!("upload: mDNS announcement for plump.local");
                return;
            }
        },
        None => socket.recv_from_with(decide).await,
    };

    if matches!(answer, Answer::Ignore) {
        return;
    }

    // RFC 6762 5.4: a query with the unicast bit set wants the answer
    // on its own port. macOS sets it on the first try, and answering
    // only the group there is a plausible way for a name to look dead
    let destination = if answer.unicast() {
        Destination::Unicast(from)
    } else {
        Destination::Multicast
    };
    debug!(
        "upload: mDNS query answered ({}, unicast={})",
        answer.as_str(),
        answer.unicast()
    );
    send_answer(socket, answer, ip, destination).await;
}

/// Where a response goes.
#[derive(Clone, Copy)]
enum Destination {
    /// The group, so every resolver on the link can cache it.
    Multicast,
    /// Back to the querier, for a question that asked for that.
    Unicast(embassy_net::IpEndpoint),
}

/// Unsolicited announcements (RFC 6762 8.3).
///
/// A responder that only ever answers questions is invisible to a
/// resolver that has already given up asking, which is the state a
/// laptop is in seconds after the device joins the network. A short
/// burst of gratuitous records on startup gets the name cached before
/// anyone looks for it.
struct Announcer {
    left: u8,
    due: Instant,
}

impl Announcer {
    fn new() -> Self {
        Self {
            left: MDNS_ANNOUNCEMENTS,
            due: Instant::now(),
        }
    }

    /// When the next announcement falls due, or `None` once the burst
    /// is spent.
    fn deadline(&self) -> Option<Instant> {
        (self.left > 0).then_some(self.due)
    }

    fn fired(&mut self) {
        self.left = self.left.saturating_sub(1);
        self.due += MDNS_ANNOUNCE_GAP;
    }
}

/// What a received message obliges us to send.
///
/// Decided from the whole question section rather than from the first
/// question at a fixed offset: resolvers routinely ask for A and AAAA
/// in one message, and put them in either order.
#[derive(Clone, Copy)]
enum Answer {
    /// Nothing addressed to us.
    Ignore,
    /// Our A record.
    Address,
    /// We own the name but have no address of that family. NSEC says
    /// so, which stops the client waiting out its IPv6 timeout.
    NoIpv6,
    /// Same as `Address` / `NoIpv6`, but the querier asked for the
    /// answer on its own port.
    AddressUnicast,
    NoIpv6Unicast,
}

impl Answer {
    fn decide(pkt: &[u8]) -> Self {
        let mut reader = DnsReader::new(pkt);
        let Some(header) = reader.header() else {
            return Self::Ignore;
        };
        if header.is_response {
            return Self::Ignore;
        }

        let mut found = Self::Ignore;
        for _ in 0..header.questions {
            let Some(q) = reader.question() else { break };
            if !q.ours || q.class != QCLASS_IN {
                continue;
            }
            match q.qtype {
                QTYPE_A | QTYPE_ANY => {
                    // an address answer beats a negative one, so it
                    // wins whatever order the questions arrived in
                    return if q.unicast {
                        Self::AddressUnicast
                    } else {
                        Self::Address
                    };
                }
                QTYPE_AAAA => {
                    found = if q.unicast {
                        Self::NoIpv6Unicast
                    } else {
                        Self::NoIpv6
                    };
                }
                _ => {}
            }
        }
        found
    }

    #[inline]
    const fn unicast(self) -> bool {
        matches!(self, Self::AddressUnicast | Self::NoIpv6Unicast)
    }

    #[inline]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Address | Self::AddressUnicast => "A",
            Self::NoIpv6 | Self::NoIpv6Unicast => "NSEC",
        }
    }
}

async fn send_answer(
    socket: &mut UdpSocket<'_>,
    answer: Answer,
    ip: [u8; 4],
    destination: Destination,
) {
    let mut resp = [0u8; MDNS_RESP_MAX];
    let len = match answer {
        Answer::Ignore => return,
        Answer::Address | Answer::AddressUnicast => build_a_response(&mut resp, ip),
        Answer::NoIpv6 | Answer::NoIpv6Unicast => build_nsec_response(&mut resp),
    };

    let endpoint = match destination {
        Destination::Multicast => embassy_net::IpEndpoint::new(
            embassy_net::IpAddress::Ipv4(Ipv4Address::from(MDNS_MULTICAST)),
            MDNS_PORT,
        ),
        Destination::Unicast(endpoint) => endpoint,
    };
    if let Err(e) = socket.send_to(&resp[..len], endpoint).await {
        debug!("upload: mDNS send failed: {:?}", e);
    }
}

/// One question from the question section.
struct Question {
    /// The qname is exactly our hostname.
    ours: bool,
    qtype: u16,
    class: u16,
    /// The querier set the unicast-response bit.
    unicast: bool,
}

struct DnsHeader {
    is_response: bool,
    questions: u16,
}

/// Cursor over a received DNS message: the read twin of [`DnsBuf`].
///
/// Names are walked label by label rather than addressed by offsets
/// from the start of the packet, so a second question is reachable and
/// a malformed one cannot read past the end of the buffer.
struct DnsReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> DnsReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn header(&mut self) -> Option<DnsHeader> {
        let _id = self.u16()?;
        let flags = self.u16()?;
        let questions = self.u16()?;
        let _answers = self.u16()?;
        let _authority = self.u16()?;
        let _additional = self.u16()?;
        Some(DnsHeader {
            is_response: flags & 0x8000 != 0,
            questions,
        })
    }

    fn question(&mut self) -> Option<Question> {
        let ours = self.match_hostname()?;
        let qtype = self.u16()?;
        let class = self.u16()?;
        Some(Question {
            ours,
            qtype,
            class: class & !MDNS_UNICAST_BIT,
            unicast: class & MDNS_UNICAST_BIT != 0,
        })
    }

    /// Consume one qname, reporting whether it is our hostname.
    ///
    /// Always consumes the whole name, match or not, so the cursor
    /// lands on the type field either way and the next question stays
    /// readable.
    fn match_hostname(&mut self) -> Option<bool> {
        // cursor into HOSTNAME_WIRE, which holds the same length-
        // prefixed labels a qname does
        let mut expect = 0usize;
        let mut same = true;

        loop {
            let len = self.u8()? as usize;
            if len & 0xC0 != 0 {
                // a compression pointer; questions do not carry one,
                // and following it would need the whole message
                return None;
            }
            if len == 0 {
                return Some(same && HOSTNAME_WIRE.get(expect) == Some(&0));
            }

            let label = self.take(len)?;
            let matches_here = HOSTNAME_WIRE.get(expect) == Some(&(len as u8))
                && HOSTNAME_WIRE
                    .get(expect + 1..expect + 1 + len)
                    .is_some_and(|want| label.eq_ignore_ascii_case(want));
            same &= matches_here;
            expect += 1 + len;
        }
    }
}

fn build_a_response(buf: &mut [u8], ip: [u8; 4]) -> usize {
    let mut w = DnsBuf::new(buf);
    put_response_header(&mut w);
    w.put(&HOSTNAME_WIRE); // name
    w.put_u16(QTYPE_A);
    w.put_u16(MDNS_CLASS_FLUSH); // CLASS IN, cache-flush
    w.put_u32(MDNS_TTL_SECS);
    w.put_u16(0x0004); // RDLENGTH
    w.put(&ip); // RDATA (IPv4 address)
    w.len()
}

/// NSEC saying the name exists but holds nothing except an A record.
fn build_nsec_response(buf: &mut [u8]) -> usize {
    // RDATA is the next owner name (ourselves, for a single record
    // set) plus one type-bitmap window: window 0, one byte, bit 1 set
    const TYPE_BITMAP: [u8; 3] = [0x00, 0x01, 0x40];

    let mut w = DnsBuf::new(buf);
    put_response_header(&mut w);
    w.put(&HOSTNAME_WIRE);
    w.put_u16(QTYPE_NSEC);
    w.put_u16(MDNS_CLASS_FLUSH);
    w.put_u32(MDNS_TTL_SECS);
    w.put_u16((HOSTNAME_WIRE.len() + TYPE_BITMAP.len()) as u16);
    w.put(&HOSTNAME_WIRE);
    w.put(&TYPE_BITMAP);
    w.len()
}

/// Header shared by every response we emit: no questions echoed, one
/// authoritative answer.
fn put_response_header(w: &mut DnsBuf<'_>) {
    w.put_u16(0x0000); // transaction ID
    w.put_u16(0x8400); // flags: response, authoritative
    w.put_u16(0x0000); // QDCOUNT
    w.put_u16(0x0001); // ANCOUNT
    w.put_u16(0x0000); // NSCOUNT
    w.put_u16(0x0000); // ARCOUNT
}

/// Wait for an input that leaves the upload screen.
///
/// Takes the manager's mapper rather than minting one: a fresh
/// `ButtonMapper::new()` is always the unswapped layout, so on a
/// left-handed device the physical Back button mapped to `PrevJump`
/// here and nothing could exit at all.
///
/// Edge navigation follows the same rule as the dispatch loop: upload
/// consumes the jump actions but only leaves when the neighbouring tab
/// exists, so Right on the last tab is a no-op instead of an exit.
async fn wait_for_exit(mapper: &ButtonMapper) -> UploadExit {
    loop {
        let hw = tasks::INPUT_EVENTS.receive().await;
        let action = match mapper.map_event(hw) {
            ActionEvent::Press(a) | ActionEvent::LongPress(a) | ActionEvent::Repeat(a) => a,
            _ => continue,
        };

        let neighbour = match action {
            Action::Back => return UploadExit::Back,
            Action::PrevJump => Tab::Upload.left(),
            Action::NextJump => Tab::Upload.right(),
            _ => continue,
        };
        if let Some(tab) = neighbour {
            return UploadExit::Tab(tab);
        }
    }
}

impl UploadScreen<'_> {
    /// Paint one upload screen: heading, caption lines, optional QR,
    /// footer, chrome.
    ///
    /// The fonts and the chrome are shared references, so copying them
    /// out leaves the screen as the only field the draw closure and
    /// the refresh contend for.
    async fn render(
        &mut self,
        lines: &[&str],
        footer: Option<&str>,
        qr: Option<&QrSymbol>,
        full_refresh: bool,
    ) {
        let heading = self.heading;
        let body = self.body;
        let chrome = self.chrome;

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

        // with a QR the text reads as its caption, so it sits at the top
        // of the band and the symbol takes what is left; without one the
        // text centres in the whole band as before
        let (body_start_y, qr_region) = match qr {
            Some(_) => {
                let qr_top = body_area_top + total_body_h + QR_GAP;
                let region = Region::new(
                    BODY_X,
                    qr_top,
                    BODY_W,
                    body_area_bottom.saturating_sub(qr_top),
                );
                (body_area_top, Some(region))
            }
            None => (
                body_area_top + body_area_h.saturating_sub(total_body_h) / 2,
                None,
            ),
        };

        let footer_region = Region::new(BODY_X, FOOTER_Y, BODY_W, body_h);

        let draw = |s: &mut StripBuffer| {
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

            if let Some(text) = footer {
                BitmapLabel::new(footer_region, text, body)
                    .alignment(Alignment::Center)
                    .draw(s)
                    .unwrap();
            }

            let theme = Theme::default_v1();
            let mut painter = Painter::new(s, &theme);

            if let (Some(symbol), Some(region)) = (qr, qr_region) {
                symbol.draw(&mut painter, region);
            }

            // same order the dispatch loop uses: chrome last, so the
            // bars win over any content pixel that strays into them
            chrome.draw_top(
                &mut painter,
                &crate::ui::TopFonts {
                    name: fonts::ui_bold_font(0),
                    small: fonts::chrome_font(),
                },
            );
        };

        let result = if full_refresh {
            self.screen.render_full(&draw).await
        } else {
            self.screen
                .render_partial(Region::new(0, 0, SCREEN_W, SCREEN_H), &draw)
                .await
        };
        if result.is_err() {
            log::warn!("upload: EPD refresh timed out");
        }
    }
}
