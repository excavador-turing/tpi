// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use clap::{builder::NonEmptyStringValueParser, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// The board answers to this over mDNS, which is what upstream's
/// documentation tells people to use. It is `pub(crate)` because `main` needs
/// to tell "the user gave us an address that is down" from "we guessed a name
/// and it does not resolve" -- two very different things to print.
#[cfg(not(feature = "localhost"))]
pub(crate) const DEFAULT_HOST_NAME: &str = "turingpi.local";
#[cfg(feature = "localhost")]
pub(crate) const DEFAULT_HOST_NAME: &str = "127.0.0.1";

/// Commandline interface that controls turing-pi's BMC. The BMC must be connected to a network
/// that is reachable over TCP/IP in order for this tool to function. All commands are persisted by
/// the BMC. Please be aware that if no hostname is specified, it will try to resolve the hostname
/// by testing a predefined sequence of options.
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true, arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Specify the Turing-pi host to connect to. Note: IPv6 addresses must be wrapped in square
    /// brackets e.g. `[::1]`
    #[arg(default_value = DEFAULT_HOST_NAME, value_parser = NonEmptyStringValueParser::new(), long, global = true, env = "TPI_HOSTNAME")]
    pub host: Option<String>,

    /// Specify a custom port to connect to.
    #[arg(long, global = true, env = "TPI_PORT")]
    pub port: Option<u16>,

    /// Specify a user name to log in as. If unused, an interactive prompt will ask for credentials
    /// unless a cached token file is present.
    #[arg(long, global = true, env = "TPI_USERNAME")]
    pub user: Option<String>,

    /// Same as `--username`
    #[arg(
        long,
        name = "PASS",
        global = true,
        env = "TPI_PASSWORD",
        hide_env_values = true
    )]
    pub password: Option<String>,

    /// Print results formatted as JSON
    #[arg(long, global = true, env = "TPI_OUTPUT_JSON")]
    pub json: bool,

    /// Force which version of the BMC API to use. Try lower the version if you are running
    /// older BMC firmware.
    #[arg(default_value = "v1-1", short, global = true)]
    pub api_version: Option<ApiVersion>,

    #[arg(short, name = "gen completion", exclusive = true)]
    pub gencompletion: Option<clap_complete::shells::Shell>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Power on/off or reset specific nodes.
    #[command(arg_required_else_help = true)]
    Power(PowerArgs),

    /// Change the USB device/host configuration. The USB-bus can only be routed to one
    /// node simultaneously.
    #[command(arg_required_else_help = true)]
    Usb(UsbArgs),

    /// Upgrade the firmware of the BMC
    #[command(arg_required_else_help = true)]
    Firmware(FirmwareArgs),

    /// Flash a given node
    #[command(arg_required_else_help = true)]
    Flash(FlashArgs),

    /// Configure the on-board Ethernet switch.
    #[command(arg_required_else_help = true)]
    Eth(EthArgs),

    /// Read or write over UART
    #[command(arg_required_else_help = true)]
    Uart(UartArgs),

    /// Advanced node modes
    #[command(arg_required_else_help = true)]
    Advanced(AdvancedArgs),

    /// Configure the cooling devices
    #[command(arg_required_else_help = true)]
    Cooling(CoolingArgs),

    #[cfg(feature = "localhost")]
    #[command(arg_required_else_help = true, hide = true)]
    Eeprom(EepromArgs),

    /// Firmware, bmcd, kernel and board identity
    About,

    /// Board temperatures
    Thermal,

    /// The board's name, as `about` reports it and mDNS advertises it
    Hostname(HostnameArgs),

    /// The time sources the board uses, and how its clock is doing on them
    Ntp(NtpArgs),

    /// Everything configured on this board, as one file
    #[command(arg_required_else_help = true)]
    Config(ConfigArgs),

    /// The read-only credential the metrics endpoint accepts
    #[command(arg_required_else_help = true)]
    Metrics(MetricsArgs),

    /// Print turing-pi info
    Info,

    /// Reboot the BMC chip. Nodes will lose power until booted!
    Reboot,
}

#[derive(ValueEnum, Clone, PartialEq, Eq)]
pub enum GetSet {
    Get,
    Set,
}

#[derive(ValueEnum, Clone, PartialEq, Eq)]
pub enum ModeCmd {
    /// Clear any advanced mode
    Normal,
    /// reboots supported compute modules and expose its eMMC storage as a mass
    /// storage device
    Msd,
}

#[derive(ValueEnum, Clone, PartialEq, Eq)]
pub enum UsbCmd {
    /// Configure the specified node as USB device. The `BMC` itself or USB-A
    /// port is USB host
    Device,
    /// Configure the specified node as USB Host. USB devices can be attached to
    /// the USB-A port on the board.
    Host,
    /// Turns the module into flashing mode and sets the USB_OTG into device
    /// mode - use to flash the module using USB_OTG port
    Flash,
    Status,
}

#[derive(ValueEnum, Clone, PartialEq, Eq)]
pub enum PowerCmd {
    On,
    Off,
    Reset,
    Status,
}

#[derive(ValueEnum, Clone, PartialEq, Eq)]
pub enum EthCmd {
    Reset,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum ApiVersion {
    V1,
    V1_1,
}

impl ApiVersion {
    pub fn scheme(&self) -> &str {
        match self {
            ApiVersion::V1 => "http",
            ApiVersion::V1_1 => "https",
        }
    }
}

#[cfg(feature = "localhost")]
#[derive(Args, Clone)]
pub struct EepromArgs {
    /// Specify command
    pub cmd: GetSet,
    pub attribute: Option<board_info::BoardInfoAttribute>,
}

#[derive(Args, Clone)]
pub struct EthArgs {
    /// Specify command
    pub cmd: EthCmd,
}

#[derive(Args)]
pub struct AdvancedArgs {
    pub mode: ModeCmd,
    /// [possible values: 1-4]
    #[arg(short, long)]
    #[arg(value_parser = clap::value_parser!(u8).range(1..5))]
    pub node: u8,
}

#[derive(Args)]
pub struct UartArgs {
    pub action: GetSet,
    /// [possible values: 1-4], Not specifying a node selects all nodes.
    #[arg(short, long)]
    #[arg(value_parser = clap::value_parser!(u8).range(1..5))]
    pub node: u8,
    #[arg(short, long)]
    pub cmd: Option<String>,
}

#[derive(Args)]
pub struct UsbArgs {
    /// specify which mode to set the given node in.
    pub mode: UsbCmd,
    /// instead of USB-A, route the USB-bus to the BMC chip.
    #[arg(short, long)]
    pub bmc: bool,
    /// [possible values: 1-4]
    #[arg(short, long)]
    #[arg(value_parser = clap::value_parser!(u8).range(1..5))]
    pub node: Option<u8>,
}

#[derive(Args, Clone)]
pub struct FirmwareArgs {
    #[command(subcommand)]
    pub cmd: Option<FirmwareCmd>,

    /// Deprecated: `tpi firmware --file X` still works and means
    /// `tpi firmware upload --file X`. Kept because it is the documented
    /// upstream spelling and is already in people's scripts.
    #[arg(short, long)]
    pub file: Option<PathBuf>,
    /// A sha256 checksum will be used by the BMC to verify the integrity
    /// of the input, in this case, the received OS image.
    #[arg(long)]
    pub sha256: Option<String>,
}

#[derive(Subcommand, Clone)]
pub enum FirmwareCmd {
    /// Upload an image from this machine and stage it
    Upload(UploadArgs),

    /// List the versions this board could install, across every source
    List(ListArgs),

    /// Install a version from the listing
    #[command(arg_required_else_help = true)]
    Install(InstallArgs),

    /// Ask whether a newer release exists.
    ///
    /// Exits 10 when one does, so `tpi firmware check || notify` works in
    /// cron without parsing output.
    Check,

    /// Where this board looks for firmware
    Sources(SourcesArgs),
}

#[derive(Args, Clone)]
pub struct UploadArgs {
    #[arg(short, long)]
    pub file: PathBuf,
    #[arg(long)]
    pub sha256: Option<String>,
    /// Put the image on the board's SD card instead of installing it.
    ///
    /// It then appears in `tpi firmware list` under the `local` source, and
    /// installing is a separate step. This is the loop for a build you are
    /// iterating on: park once, install, reboot, park the next one -- without
    /// each upload arming the board the moment it lands.
    #[arg(long)]
    pub park: bool,
}

#[derive(Args, Clone)]
pub struct ListArgs {
    /// Re-poll the sources instead of using the board's cached answer.
    #[arg(long)]
    pub refresh: bool,
    /// Include versions that are older than, or unrelated to, the running
    /// one. The default listing is what you could move TO.
    ///
    /// Long-only: `-a` is already the global `--api-version`, and clap's own
    /// debug assertion catches the clash by panicking -- which is how this
    /// was found, because `firmware list --all` had never been run.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Clone)]
pub struct InstallArgs {
    /// A version from `tpi firmware list`, e.g. v2.8.0
    /// The version to install, as `firmware list` shows it.
    ///
    /// Named `target` inside because `version` collides with the `--version`
    /// flag clap generates for every subcommand -- a collision clap's debug
    /// assertion reports by panicking, and a release build resolves by
    /// undefined argument handling. `firmware install` had never worked in
    /// any release because of it.
    #[arg(value_name = "VERSION")]
    pub target: String,
    /// Which source to take it from. Only needed when more than one offers
    /// the same version.
    #[arg(short, long)]
    pub source: Option<String>,
    /// Replace an update that is already staged for the next boot.
    #[arg(long)]
    pub force: bool,
    /// Do not ask for confirmation.
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args, Clone)]
pub struct SourcesArgs {
    #[command(subcommand)]
    pub cmd: SourcesCmd,
}

#[derive(Subcommand, Clone)]
pub enum SourcesCmd {
    /// Show the configured sources
    List,
    /// Add a source
    #[command(arg_required_else_help = true)]
    Add(SourceAddArgs),
    /// Remove a source by id
    #[command(arg_required_else_help = true)]
    Remove { id: String },
    /// Start consulting a source again
    #[command(arg_required_else_help = true)]
    Enable { id: String },
    /// Stop consulting a source without deleting it
    #[command(arg_required_else_help = true)]
    Disable { id: String },
}

#[derive(Args, Clone)]
pub struct SourceAddArgs {
    /// Stable identifier. The install call refers to this, not the label,
    /// so renaming a source later cannot change what an install means.
    pub id: String,
    /// `owner/repo` for github, a URL prefix for http, a path for local
    pub location: String,
    #[arg(short, long, value_enum, default_value_t = SourceKindArg::Github)]
    pub kind: SourceKindArg,
    /// What to call it in listings. Defaults to the id.
    #[arg(short, long)]
    pub label: Option<String>,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum SourceKindArg {
    Github,
    Http,
    Local,
}

#[derive(Args, Clone)]
pub struct HostnameArgs {
    /// The new name. Omit to print the current one.
    ///
    /// One DNS label: letters, digits and hyphens, and no dots. Changing it
    /// moves the `instance` label on every metrics series, so a Prometheus
    /// history does not follow the board across the rename.
    pub name: Option<String>,
}

#[derive(Args, Clone)]
pub struct NtpArgs {
    #[command(subcommand)]
    pub cmd: Option<NtpCmd>,
}

#[derive(Subcommand, Clone)]
pub enum NtpCmd {
    /// Print the configured servers and the state of the clock
    Show,
    /// Replace the servers, in preference order
    ///
    /// The first is written with chrony's `prefer`, so a LAN source wins over
    /// a pool that happens to answer faster. Pass none to go back to the
    /// image's own pool.
    Set {
        #[arg(value_name = "SERVER")]
        servers: Vec<String>,
    },
}

#[derive(Args, Clone)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub cmd: ConfigCmd,
}

#[derive(Subcommand, Clone)]
pub enum ConfigCmd {
    /// Write this board's settings to a file, or to stdout
    Export {
        /// Where to write it. Omit for stdout.
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Include the metrics token.
        ///
        /// This makes the file a credential: applied to another board it can
        /// scrape it. Without this the token is absent from the document
        /// entirely, not present and empty.
        #[arg(long)]
        with_secrets: bool,
    },
    /// Apply a previously exported file to this board
    Import {
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Apply without asking.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args, Clone)]
pub struct MetricsArgs {
    #[command(subcommand)]
    pub cmd: MetricsCmd,
}

#[derive(Subcommand, Clone)]
pub enum MetricsCmd {
    /// Print the current token
    Show,
    /// Replace the token. Anything scraping with the old one stops.
    Rotate,
}

#[derive(Args, Clone)]
#[group(required = true)]
pub struct FlashArgs {
    /// Update a node with an image accessible from the local filesystem,
    /// typically a BMC-visible microSD card.
    #[arg(short, long)]
    pub local: bool,
    /// Update a node with the given image.
    #[arg(short, long)]
    pub image_path: PathBuf,
    /// [possible values: 1-4]
    #[arg(short, long)]
    #[arg(value_parser = clap::value_parser!(u8).range(1..5))]
    pub node: u8,
    /// A sha256 checksum will be used by the BMC to verify the integrity
    /// of the input, in this case, the received OS image.
    #[arg(long)]
    pub sha256: Option<String>,
    /// Opt out of the crc integrity check. This is check is not responsible for
    /// the sha256 validation. But validates the written areas on the node with
    /// a crc digest. Skipping this step will reduce the overall time
    /// but permits corrupted written data.
    #[arg(long)]
    pub skip_crc: bool,
}

#[derive(Args)]
pub struct PowerArgs {
    /// Specify command
    pub cmd: PowerCmd,
    /// [possible values: 1-4], Not specifying a node selects all nodes.
    #[arg(short, long)]
    #[arg(value_parser = clap::value_parser!(u8).range(1..5))]
    pub node: Option<u8>,
}

#[derive(Args, Clone)]
pub struct CoolingArgs {
    /// Specify command
    pub cmd: CoolingCmd,
    /// Specify the cooling device (required for set command)
    pub device: Option<String>,
    /// Specify the cooling device speed (required for set command)
    pub speed: Option<u32>,
    /// Hold the speed: pause the zone's governor so the step stays put
    ///
    /// Without this the kernel takes the fan back at its next poll, a few
    /// seconds later, because the governor is what decides the step.
    #[arg(long, visible_alias = "override")]
    pub hold: bool,
    /// Hand the fan back to the kernel's governor
    ///
    /// Clears a hold. No speed is needed, because the governor is about to
    /// choose one.
    #[arg(long, conflicts_with = "hold")]
    pub auto: bool,
}

#[derive(ValueEnum, Clone, PartialEq, Eq)]
pub enum CoolingCmd {
    Set,
    Status,
}
