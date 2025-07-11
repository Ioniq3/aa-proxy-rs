mod aoa;
mod bluetooth;
mod io_uring;
mod mitm;
mod usb_gadget;
mod usb_stream;

use bluer::Address;
use bluetooth::bluetooth_setup_connection;
use bluetooth::bluetooth_stop;
use clap::Parser;
use humantime::format_duration;
use io_uring::io_loop;
use simple_config_parser::Config;
use simplelog::*;
use usb_gadget::uevent_listener;
use usb_gadget::UsbGadgetState;

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Builder;
use tokio::sync::Notify;
use tokio::time::Instant;

// module name for logging engine
const NAME: &str = "<i><bright-black> main: </>";

const DEFAULT_WLAN_ADDR: &str = "10.0.0.1";
const TCP_SERVER_PORT: i32 = 5288;
const TCP_DHU_PORT: i32 = 5277;

#[derive(clap::ValueEnum, Default, Debug, PartialEq, PartialOrd, Clone, Copy)]
pub enum HexdumpLevel {
    #[default]
    Disabled,
    DecryptedInput,
    RawInput,
    DecryptedOutput,
    RawOutput,
    All,
}

#[derive(Debug, Clone)]
struct UsbId {
    vid: u16,
    pid: u16,
}

impl std::str::FromStr for UsbId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 2 {
            return Err("Expected format VID:PID".to_string());
        }
        let vid = u16::from_str_radix(parts[0], 16).map_err(|e| e.to_string())?;
        let pid = u16::from_str_radix(parts[1], 16).map_err(|e| e.to_string())?;
        Ok(UsbId { vid, pid })
    }
}

/// AndroidAuto wired/wireless proxy
#[derive(Parser, Debug)]
#[clap(version, long_about = None, about = format!(
    "🛸 aa-proxy-rs, build: {}, git: {}-{}",
    env!("BUILD_DATE"),
    env!("GIT_DATE"),
    env!("GIT_HASH")
))]
struct Args {
    /// BLE advertising
    #[clap(short, long)]
    advertise: bool,

    /// Enable debug info
    #[clap(short, long)]
    debug: bool,

    /// Hex dump level
    #[clap(long, default_value_t, value_enum, requires("debug"))]
    hexdump_level: HexdumpLevel,

    /// Enable legacy mode
    #[clap(short, long)]
    legacy: bool,

    /// Auto-connect to saved phone or specified phone MAC address if provided
    #[clap(short, long, num_args(..=1), default_missing_value("00:00:00:00:00:00"))]
    connect: Option<Address>,

    /// Log file path
    #[clap(long, value_parser, default_value = "/var/log/aa-proxy-rs.log")]
    logfile: PathBuf,

    /// Interval of showing data transfer statistics (0 = disabled)
    #[clap(short, long, value_name = "SECONDS", default_value_t = 0)]
    stats_interval: u16,

    /// UDC Controller name
    #[clap(short, long)]
    udc: Option<String>,

    /// WLAN / Wi-Fi Hotspot interface
    #[clap(short, long, default_value = "wlan0")]
    iface: String,

    /// hostapd.conf file location
    #[clap(long, value_parser, default_value = "/var/run/hostapd.conf")]
    hostapd_conf: PathBuf,

    /// BLE device name
    #[clap(short, long)]
    btalias: Option<String>,

    /// Keep alive mode: BLE adapter doesn't turn off after successful connection,
    /// so that the phone can remain connected (used in special configurations)
    #[clap(short, long)]
    keepalive: bool,

    /// Data transfer timeout
    #[clap(short, long, value_name = "SECONDS", default_value_t = 10)]
    timeout_secs: u16,

    /// Enable MITM mode (experimental)
    #[clap(short, long)]
    mitm: bool,

    /// MITM: Force DPI (experimental)
    #[clap(long, requires("mitm"))]
    dpi: Option<u16>,

    /// MITM: remove tap restriction
    #[clap(long, requires("mitm"))]
    remove_tap_restriction: bool,

    /// MITM: video in motion
    #[clap(long, requires("mitm"))]
    video_in_motion: bool,

    /// MITM: Disable media sink
    #[clap(long, requires("mitm"))]
    disable_media_sink: bool,

    /// MITM: Disable TTS sink
    #[clap(long, requires("mitm"))]
    disable_tts_sink: bool,

    /// MITM: Developer mode
    #[clap(long, requires("mitm"))]
    developer_mode: bool,

    /// Enable wired USB connection with phone (optional VID:PID can be specified, zero is wildcard)
    #[clap(short, long, value_parser, num_args(..=1), default_missing_value("0000:0000"))]
    wired: Option<UsbId>,

    /// Use a Google Android Auto Desktop Head Unit emulator
    /// instead of real HU device (will listen on TCP 5277 port)
    #[clap(long)]
    dhu: bool,
}

#[derive(Clone)]
struct WifiConfig {
    ip_addr: String,
    port: i32,
    ssid: String,
    bssid: String,
    wpa_key: String,
}

fn init_wifi_config(iface: &str, hostapd_conf: PathBuf) -> WifiConfig {
    let mut ip_addr = String::from(DEFAULT_WLAN_ADDR);

    // Get UP interface and IP
    for ifa in netif::up().unwrap() {
        match ifa.name() {
            val if val == iface => {
                debug!("Found interface: {:?}", ifa);
                // IPv4 Address contains None scope_id, while IPv6 contains Some
                match ifa.scope_id() {
                    None => {
                        ip_addr = ifa.address().to_string();
                        break;
                    }
                    _ => (),
                }
            }
            _ => (),
        }
    }

    let bssid = mac_address::mac_address_by_name(iface)
        .unwrap()
        .unwrap()
        .to_string();

    // Create a new config from hostapd.conf
    let hostapd = Config::new().file(hostapd_conf).unwrap();

    // read SSID and WPA_KEY
    let ssid = &hostapd.get_str("ssid").unwrap();
    let wpa_key = &hostapd.get_str("wpa_passphrase").unwrap();

    WifiConfig {
        ip_addr,
        port: TCP_SERVER_PORT,
        ssid: ssid.into(),
        bssid,
        wpa_key: wpa_key.into(),
    }
}

fn logging_init(debug: bool, log_path: &PathBuf) {
    let conf = ConfigBuilder::new()
        .set_time_format("%F, %H:%M:%S%.3f".to_string())
        .set_write_log_enable_colors(true)
        .build();

    let mut loggers = vec![];

    let requested_level = if debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let console_logger: Box<dyn SharedLogger> = TermLogger::new(
        requested_level,
        conf.clone(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );
    loggers.push(console_logger);

    let mut logfile_error: Option<String> = None;
    let logfile = OpenOptions::new().create(true).append(true).open(&log_path);
    match logfile {
        Ok(logfile) => {
            loggers.push(WriteLogger::new(requested_level, conf, logfile));
        }
        Err(e) => {
            logfile_error = Some(format!(
                "Error creating/opening log file: {:?}: {:?}",
                log_path, e
            ));
        }
    }

    CombinedLogger::init(loggers).expect("Cannot initialize logging subsystem");
    if logfile_error.is_some() {
        error!("{} {}", NAME, logfile_error.unwrap());
        warn!("{} Will do console logging only...", NAME);
    }
}

async fn tokio_main(args: Args, need_restart: Arc<Notify>, tcp_start: Arc<Notify>) {
    let accessory_started = Arc::new(Notify::new());
    let accessory_started_cloned = accessory_started.clone();

    let wifi_conf = {
        if !args.wired.is_some() {
            Some(init_wifi_config(&args.iface, args.hostapd_conf))
        } else {
            None
        }
    };
    let mut usb = None;
    if !args.dhu {
        if args.legacy {
            // start uevent listener in own task
            std::thread::spawn(|| uevent_listener(accessory_started_cloned));
        }
        usb = Some(UsbGadgetState::new(args.legacy, args.udc));
    }
    loop {
        if let Some(ref mut usb) = usb {
            if let Err(e) = usb.init() {
                error!("{} 🔌 USB init error: {}", NAME, e);
            }
        }

        let mut bt_stop = None;
        if let Some(ref wifi_conf) = wifi_conf {
            loop {
                match bluetooth_setup_connection(
                    args.advertise,
                    args.btalias.clone(),
                    args.connect,
                    wifi_conf.clone(),
                    tcp_start.clone(),
                    args.keepalive,
                )
                .await
                {
                    Ok(state) => {
                        // we're ready, gracefully shutdown bluetooth in task
                        bt_stop = Some(tokio::spawn(async move { bluetooth_stop(state).await }));
                        break;
                    }
                    Err(e) => {
                        error!("{} Bluetooth error: {}", NAME, e);
                        info!("{} Trying to recover...", NAME);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        }

        if let Some(ref mut usb) = usb {
            usb.enable_default_and_wait_for_accessory(accessory_started.clone())
                .await;
        }

        if let Some(bt_stop) = bt_stop {
            // wait for bluetooth stop properly
            let _ = bt_stop.await;
        }

        // wait for restart
        need_restart.notified().await;

        // TODO: make proper main loop with cancelation
        info!(
            "{} 📵 TCP/USB connection closed or not started, trying again...",
            NAME
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

fn main() {
    let started = Instant::now();
    let args = Args::parse();
    logging_init(args.debug, &args.logfile);

    let stats_interval = {
        if args.stats_interval == 0 {
            None
        } else {
            Some(Duration::from_secs(args.stats_interval.into()))
        }
    };
    let read_timeout = Duration::from_secs(args.timeout_secs.into());

    info!(
        "🛸 <b><blue>aa-proxy-rs</> is starting, build: {}, git: {}-{}",
        env!("BUILD_DATE"),
        env!("GIT_DATE"),
        env!("GIT_HASH")
    );
    if let Some(ref wired) = args.wired {
        info!(
            "{} 🔌 enabled wired USB connection with {:04X?}",
            NAME, wired
        );
    }
    info!(
        "{} 📜 Log file path: <b><green>{}</>",
        NAME,
        args.logfile.display()
    );
    info!(
        "{} ⚙️ Showing transfer statistics: <b><blue>{}</>",
        NAME,
        match stats_interval {
            Some(d) => format_duration(d).to_string(),
            None => "disabled".to_string(),
        }
    );

    // notify for syncing threads
    let need_restart = Arc::new(Notify::new());
    let need_restart_cloned = need_restart.clone();
    let tcp_start = Arc::new(Notify::new());
    let tcp_start_cloned = tcp_start.clone();
    let mitm = args.mitm;
    let dpi = args.dpi;
    let developer_mode = args.developer_mode;
    let disable_media_sink = args.disable_media_sink;
    let disable_tts_sink = args.disable_tts_sink;
    let remove_tap_restriction = args.remove_tap_restriction;
    let video_in_motion = args.video_in_motion;
    let hex_requested = args.hexdump_level;
    let wired = args.wired.clone();
    let dhu = args.dhu;

    // build and spawn main tokio runtime
    let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.spawn(async move { tokio_main(args, need_restart, tcp_start).await });

    // start tokio_uring runtime simultaneously
    let _ = tokio_uring::start(io_loop(
        stats_interval,
        need_restart_cloned,
        tcp_start_cloned,
        read_timeout,
        mitm,
        dpi,
        developer_mode,
        disable_media_sink,
        disable_tts_sink,
        remove_tap_restriction,
        video_in_motion,
        hex_requested,
        wired,
        dhu,
    ));

    info!(
        "🚩 aa-proxy-rs terminated, running time: {}",
        format_duration(started.elapsed()).to_string()
    );
}
