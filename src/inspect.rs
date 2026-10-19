//! Read-only diagnostic snapshot; never emit guest cmdline or seed contents.
use crate::{
    config::HostConfig,
    error::{Error, Result},
    hypervisor,
    instance::{self, BootConfig, InstanceInfo},
    metadata::Lock,
    output::json_string,
};
use std::path::PathBuf;

pub struct Inspection {
    pub instance: InstanceInfo,
    pub config_toml: String,
    pub firmware: Option<PathBuf>,
    pub seed_path: Option<PathBuf>,
    pub program: Option<PathBuf>,
    pub args: Vec<String>,
    pub command_error: Option<String>,
}

pub fn inspect(host: &HostConfig, name: &str) -> Result<Inspection> {
    host.validate()?;
    crate::metadata::validate_name("instance", name)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("instance {name:?} was not found")));
    };
    let config = instance::load(host, name)?;
    let firmware = match &config.boot {
        BootConfig::Firmware { .. } => Some(host.firmware_path()),
        BootConfig::Direct { .. } => None,
    };
    let mut redacted = config.clone();
    if let BootConfig::Direct { cmdline, .. } = &mut redacted.boot {
        *cmdline = "<redacted>".into();
    }
    let config_toml = toml::to_string_pretty(&redacted)
        .map_err(|e| Error::Resource(format!("cannot serialize inspected configuration: {e}")))?;
    let seed_path = config
        .cloud_init
        .as_ref()
        .and_then(|seed| crate::cloud_init::resolve(host, seed).ok());
    // Resolve paths as launch would, but never construct an output containing the secret cmdline.
    let (program, args, command_error) = match hypervisor::command_line(host, &redacted) {
        Ok((program, args)) => (
            Some(program),
            args.into_iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            None,
        ),
        Err(e) => (None, Vec::new(), Some(e.to_string())),
    };
    Ok(Inspection {
        instance: instance::info(host, config),
        config_toml,
        firmware,
        seed_path,
        program,
        args,
        command_error,
    })
}
fn path(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|p| json_string(&p.to_string_lossy()))
        .unwrap_or_else(|| "null".into())
}
impl Inspection {
    pub fn json(&self) -> String {
        format!("{{\"instance\":{},\"resolved_config_toml\":{},\"firmware\":{},\"cloud_init_path\":{},\"command_line\":{{\"program\":{},\"args\":[{}],\"error\":{},\"cmdline_redacted\":{}}}}}", self.instance.json(), json_string(&self.config_toml), path(&self.firmware), path(&self.seed_path), path(&self.program), self.args.iter().map(|arg| json_string(arg)).collect::<Vec<_>>().join(","), self.command_error.as_ref().map(|e|json_string(e)).unwrap_or_else(||"null".into()), matches!(self.instance.config.boot, BootConfig::Direct {..}))
    }
    pub fn text(&self) -> String {
        let mut text = format!("Name: {}\nState: {} ({})\n\nResolved configuration (sensitive values redacted):\n{}\nDisk paths:\n", self.instance.config.name, self.instance.state, self.instance.state_source, self.config_toml);
        for disk in &self.instance.config.disk {
            let resolved = self
                .instance
                .disks
                .iter()
                .find(|volume| volume.config.id == disk.volume_id)
                .and_then(|volume| volume.path.as_ref());
            text.push_str(&format!(
                "  {}: {}\n",
                disk.name,
                resolved
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(unresolved)".into())
            ));
        }
        if let Some(seed) = &self.seed_path {
            text.push_str(&format!("  cloud-init (readonly): {}\n", seed.display()));
        }
        text.push_str("\nTAPs:\n");
        for net in &self.instance.config.network {
            text.push_str(&format!(
                "  {}: bridge={}, mac={}\n",
                net.tap, net.bridge, net.mac
            ));
        }
        text.push_str(&format!(
            "\nRuntime:\n  Unit: {}\n  API socket: {}\n  Console socket: {}\n",
            self.instance.unit,
            self.instance.api_socket.display(),
            self.instance.console_socket.display()
        ));
        if let Some(firmware) = &self.firmware {
            text.push_str(&format!("  Firmware: {}\n", firmware.display()));
        }
        text.push_str(
            "\nCloud Hypervisor launch arguments (JSON array; sensitive values redacted):\n",
        );
        if let Some(program) = &self.program {
            let argv = std::iter::once(json_string(&program.to_string_lossy()))
                .chain(self.args.iter().map(|arg| json_string(arg)))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("  [{argv}]\n"));
        } else {
            text.push_str(&format!(
                "  unavailable: {}\n",
                self.command_error.as_deref().unwrap_or("unknown reason")
            ));
        }
        if let Some(reason) = &self.instance.reason {
            text.push_str(&format!("\nAvailability: {}\n", reason));
        }
        text
    }
}
