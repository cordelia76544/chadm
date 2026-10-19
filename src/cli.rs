use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "chadm",
    version,
    about = "Manage Cloud Hypervisor virtual machines"
)]
pub struct Cli {
    /// Read an explicit host TOML configuration (must exist).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(name = "__service", hide = true)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Print the chadm version.
    Version,
    /// Diagnose this host without changing its configuration.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Inspect resolved configuration and redacted launch arguments without changes.
    Inspect {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Manage instances, disks and systemd lifecycles.
    Instance {
        #[command(subcommand)]
        action: InstanceAction,
    },
    /// Manage storage repositories.
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
    /// Import and manage immutable base images.
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
    /// Manage independent storage volumes.
    Volume {
        #[command(subcommand)]
        action: VolumeAction,
    },
    /// Manage stopped-instance disk snapshots and saved suspend checkpoints.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum SnapshotAction {
    /// Snapshot all writable disks of a stopped instance.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        instance: String,
    },
    /// Restore all disks in place; instance must be stopped with unchanged config.
    Restore { name: String },
    /// Delete snapshot payloads; --discard explicitly discards suspend execution state.
    Delete {
        name: String,
        #[arg(long)]
        discard: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum VolumeAction {
    /// Create a blank raw sparse file or zvol in a repo.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        repo: String,
        #[arg(long, value_parser = crate::volume::parse_size)]
        size: u64,
    },
    /// Attach a data volume; hotplug when running, persist for next start when stopped.
    Attach {
        name: String,
        #[arg(long)]
        instance: String,
    },
    /// Detach a data volume, waiting for Guest PCI eject when running; retain its contents.
    Detach {
        name: String,
        #[arg(long)]
        instance: String,
    },
    /// Delete a validated, unreferenced managed volume.
    Delete { name: String },
    /// List configured volumes.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one volume and its availability.
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RepoDriver {
    File,
    Zfs,
}

#[derive(Debug, Subcommand)]
pub enum RepoAction {
    /// Create a managed file or ZFS storage repository.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, value_enum)]
        driver: RepoDriver,
        /// Repository purpose; cloudinit requires the reserved name cloudinit.
        #[arg(long, value_enum)]
        kind: Option<crate::repo::RepoKind>,
        #[arg(long, required_if_eq("driver", "file"), conflicts_with = "pool")]
        path: Option<PathBuf>,
        /// New dataset below an existing filesystem, for example tank/chadm.
        #[arg(long, required_if_eq("driver", "zfs"), conflicts_with = "path")]
        pool: Option<String>,
    },
    /// Delete an empty managed repo, retaining its root directory.
    Delete { name: String },
    /// List configured repositories.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one repository and its availability.
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ImageAction {
    /// Import a local raw or standalone qcow2 image as read-only raw.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        path: PathBuf,
        /// Mark an installed Windows image; instances inherit the Windows CPU profile.
        #[arg(long)]
        windows: bool,
    },
    /// Delete an unreferenced, validated managed image.
    Delete { name: String },
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BootMode {
    #[value(alias = "fireware")]
    Firmware,
    Direct,
}

#[derive(Debug, clap::Args)]
pub struct InstanceCreate {
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub image: String,
    /// Destination for new volumes; defaults to the image's repo.
    #[arg(long)]
    pub repo: Option<String>,
    /// Select a raw seed image in the reserved file repo named cloudinit.
    #[arg(long, conflicts_with = "no_cloud_init")]
    pub cloud_init: Option<String>,
    /// Skip the default seed, even when the cloudinit repo exists.
    #[arg(long)]
    pub no_cloud_init: bool,
    #[arg(long, value_enum, default_value = "firmware")]
    pub boot: BootMode,
    #[arg(long, default_value = "2", value_parser = clap::value_parser!(u16).range(1..))]
    pub cpus: u16,
    #[arg(long, default_value = "2G", value_parser = crate::volume::parse_size)]
    pub ram: u64,
    #[arg(long, required_if_eq("boot", "direct"))]
    pub kernel: Option<PathBuf>,
    #[arg(long, requires = "kernel")]
    pub initramfs: Option<PathBuf>,
    #[arg(long, required_if_eq("boot", "direct"))]
    pub cmdline: Option<String>,
    #[arg(long = "volume", value_parser = crate::instance::parse_volume)]
    pub volumes: Vec<crate::instance::VolumeSpec>,
    /// Linux bridge network; DHCP is the default, e.g. bridge=virbr0.
    #[arg(long = "net", value_parser = crate::instance::parse_network)]
    pub networks: Vec<crate::instance::NetworkSpec>,
}
#[derive(Debug, Subcommand)]
pub enum InstanceAction {
    /// Save RAM/CPU/device state and disk checkpoints, then terminate the VMM.
    Suspend { name: String },
    /// Restore a disk-backed suspend checkpoint and continue execution.
    Resume { name: String },
    /// Clean owned stale runtime/TAPs only after systemd proves the VM is stopped.
    Recover { name: String },
    /// Connect to the serial console; Ctrl+] disconnects without stopping the VM.
    Console { name: String },
    /// Prepare networking and start the shared systemd template instance.
    Start { name: String },
    /// Request Guest shutdown; --force terminates the unit cgroup.
    Stop {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Stop then start; --force skips Guest graceful shutdown.
    Reboot {
        name: String,
        #[arg(long)]
        force: bool,
    },

    /// Create metadata and independent root/data volumes; do not start a VM.
    Create(Box<InstanceCreate>),
    /// Delete metadata only, retaining all volumes.
    Delete { name: String },
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    Prepare { name: String },
    Run { name: String },
    Ready { name: String },
    Cleanup { name: String },
}
