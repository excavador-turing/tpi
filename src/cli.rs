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

    /// Uptime, load, memory, NAND wear and the state of the clock
    Health,

    /// What is on the microSD card, and which of it can be flashed to a node
    Sdcard(SdcardArgs),

    /// The board's name, as `about` reports it and mDNS advertises it
    Hostname(HostnameArgs),

    /// The time sources the board uses, and how its clock is doing on them
    Ntp(NtpArgs),

    /// Everything configured on this board, as one file
    #[command(arg_required_else_help = true)]
    Config(ConfigArgs),

    /// The certificate this board serves over HTTPS
    #[command(arg_required_else_help = true)]
    Tls(TlsArgs),

    /// The on-board Ethernet switch: which ports can talk to which
    #[command(arg_required_else_help = true)]
    Network(NetworkArgs),

    /// Print turing-pi info
    Info,

    /// Reboot the BMC. The compute modules keep running throughout.
    ///
    /// Only this interface, the API and the consoles go away, for about half
    /// a minute. The daemon's own summary says the same thing, and it has
    /// been measured on every firmware flash this estate has done: all four
    /// modules stayed powered and their Kubernetes nodes never restarted.
    ///
    /// This help used to say the nodes would lose power. They do not, and a
    /// warning that overstates the blast radius talks people out of a reboot
    /// they should do.
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
    /// One DNS label: letters, digits and hyphens, and no dots.
    ///
    /// The board's name does not appear in its metrics, so renaming it moves
    /// no history. If your scraper labels targets by hostname, that is where
    /// to change it.
    pub name: Option<String>,
}

#[derive(Args, Clone)]
pub struct NetworkArgs {
    #[command(subcommand)]
    pub cmd: NetworkCmd,
}

#[derive(Subcommand, Clone)]
pub enum NetworkCmd {
    /// The on-board switch
    #[command(arg_required_else_help = true)]
    Switch(SwitchArgs),

    /// The BMC's own address: DHCP or static, applied then confirmed
    #[command(arg_required_else_help = true)]
    Address(AddressArgs),
}

#[derive(Args, Clone)]
pub struct AddressArgs {
    #[command(subcommand)]
    pub cmd: AddressCmd,
}

#[derive(Subcommand, Clone)]
pub enum AddressCmd {
    /// What the board is on, what a reboot comes back to, what the bridge
    /// actually has, and anything waiting to be confirmed.
    Show,

    /// Apply an address, to be confirmed AT that address.
    ///
    /// The address goes on the bridge and is NOT kept. Confirm within the
    /// window -- from a machine that reaches the board at the new address --
    /// or the board puts the previous one back by itself. The bridge and the
    /// modules' ports are never brought down; only the address changes.
    Apply(AddressApplyArgs),

    /// Keep an address that was applied.
    ///
    /// Run this as a NEW invocation, against the NEW address. That is the
    /// proof: if this command reaches the board, the address works.
    Confirm(AddressConfirmArgs),

    /// Put a pending address back now, rather than waiting out its window.
    Revert,
}

#[derive(Args, Clone)]
pub struct AddressApplyArgs {
    /// Ask the network for an address (the image's default).
    #[arg(long, conflicts_with_all = ["static_addr", "gateway", "dns", "search"])]
    pub dhcp: bool,

    /// A fixed address with its prefix, e.g. 192.168.1.20/24.
    #[arg(
        long = "static",
        value_name = "ADDRESS/PREFIX",
        conflicts_with = "dhcp"
    )]
    pub static_addr: Option<String>,

    /// The gateway, on the same subnet. Optional; without one the board
    /// reaches its own subnet and nothing beyond.
    #[arg(long, value_name = "ADDRESS")]
    pub gateway: Option<std::net::Ipv4Addr>,

    /// A resolver; repeat for more, in order. Without one names do not
    /// resolve on the board, `pool.ntp.org` included.
    #[arg(long, value_name = "ADDRESS")]
    pub dns: Vec<std::net::Ipv4Addr>,

    /// A search domain for the resolver.
    #[arg(long, value_name = "DOMAIN")]
    pub search: Option<String>,

    /// Seconds to confirm within. The board's default is 30.
    #[arg(long, value_name = "SECONDS")]
    pub window: Option<u64>,
}

#[derive(Args, Clone)]
pub struct AddressConfirmArgs {
    /// The token the apply printed.
    pub token: String,
}

#[derive(Args, Clone)]
pub struct SwitchArgs {
    #[command(subcommand)]
    pub cmd: SwitchCmd,
}

#[derive(Subcommand, Clone)]
pub enum SwitchCmd {
    /// What the switch is running, what was last confirmed, and anything
    /// waiting to be confirmed.
    Show(SwitchShowArgs),

    /// The presets the board offers, expanded by the board itself.
    ///
    /// The board expands them, never this tool: a client that expanded a
    /// preset itself would eventually disagree with the board about what it
    /// means, and that disagreement is a board nobody can reach.
    Presets,

    /// Apply a configuration, to be confirmed.
    ///
    /// The change goes on the switch and is NOT kept. Confirm it within the
    /// window or the board puts the previous configuration back by itself.
    ///
    /// The window does not start counting until the uplink carrying the BMC's
    /// VLAN forwards, because spanning tree holds a port for its own
    /// forwarding delay first.
    Apply(SwitchApplyArgs),

    /// Keep a change that was applied.
    ///
    /// Run this as a NEW invocation after the apply. That is the proof: if
    /// this command can reach the board, the new configuration works.
    Confirm(SwitchConfirmArgs),

    /// Put a pending change back now, rather than waiting out its window.
    Revert,
}

#[derive(Args, Clone)]
pub struct SwitchShowArgs {
    /// Print the running document alone, as JSON, and nothing else.
    ///
    /// This is the other half of `apply --table`, and the two are meant to be
    /// used together:
    ///
    ///     tpi network switch show --table > mine.json
    ///     $EDITOR mine.json
    ///     tpi network switch apply --table mine.json
    ///
    /// Starting from what the board is actually running, rather than from a
    /// blank file, means the ports you did not mean to change keep the
    /// configuration they already had.
    #[arg(long)]
    pub table: bool,
}

#[derive(Args, Clone)]
pub struct SwitchApplyArgs {
    /// flat, split or trunk. Mutually exclusive with --table.
    #[arg(long, value_name = "NAME", conflicts_with = "table")]
    pub preset: Option<SwitchPresetArg>,

    /// A whole document as JSON, for anything the presets do not cover.
    #[arg(long, value_name = "FILE", conflicts_with = "preset")]
    pub table: Option<PathBuf>,

    /// Trunk only: the VLAN the BMC is on. Yours, because your router has to
    /// match it.
    #[arg(long, value_name = "VID")]
    pub mgmt_vid: Option<u16>,

    /// Trunk only: the VLAN the modules are on.
    #[arg(long, value_name = "VID")]
    pub node_vid: Option<u16>,

    /// Trunk only. `redundant` gives ge1 the same VLANs as ge0 under spanning
    /// tree; `off` leaves ge1 carrying nothing.
    #[arg(long, value_name = "MODE")]
    pub second_uplink: Option<SecondUplinkArg>,

    /// Seconds to confirm within. The board's default is 30.
    #[arg(long, value_name = "SECONDS")]
    pub window: Option<u64>,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum SwitchPresetArg {
    /// One network: every module, the BMC and both uplinks share it.
    Flat,
    /// Two networks that never meet: the BMC out of ge0, the modules out of
    /// ge1, nothing tagged.
    Split,
    /// One cable carrying both, tagged, for a router that knows the VLANs.
    Trunk,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum SecondUplinkArg {
    /// ge1 carries what ge0 carries, and spanning tree picks one.
    Redundant,
    /// ge1 carries nothing.
    Off,
}

#[derive(Args, Clone)]
pub struct SwitchConfirmArgs {
    /// The token the apply printed.
    pub token: String,
}

#[derive(Args, Clone)]
pub struct TlsArgs {
    #[command(subcommand)]
    pub cmd: TlsCmd,
}

#[derive(Subcommand, Clone)]
pub enum TlsCmd {
    /// What the board serves now: subject, issuer, expiry, key and the names
    /// it asserts.
    Show,

    /// Install a certificate and its key.
    ///
    /// For a board that should serve a certificate from your own CA, so a
    /// browser that trusts that CA opens it without a warning -- and the
    /// serial console works, which a click-through exception does not cover.
    ///
    /// Takes effect on the next connection. The board is not restarted and
    /// existing sessions are not dropped.
    Install(TlsInstallArgs),

    /// Remove an installed certificate and let the board issue its own.
    ///
    /// The board is never left without a certificate. Refused when it is
    /// already serving its own.
    Reset,
}

#[derive(Args, Clone)]
pub struct TlsInstallArgs {
    /// The certificate, PEM. Intermediates may follow the leaf in the same
    /// file, leaf first, which is the order every tool writes them.
    #[arg(long, value_name = "FILE")]
    pub cert: PathBuf,

    /// Its private key, PEM.
    ///
    /// Read and sent; never printed, and never written to the board's log.
    #[arg(long, value_name = "FILE")]
    pub key: PathBuf,
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

/// `tpi sdcard` — what is on the card.
#[derive(Args, Debug)]
pub struct SdcardArgs {
    /// A directory on the card, relative to its root. Defaults to the root.
    ///
    /// Always relative and always confined to the card: the daemon refuses
    /// anything that resolves outside it, symlinks included.
    pub path: Option<String>,
}
