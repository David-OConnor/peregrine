use std::{
    collections::HashSet,
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use super::{CpuTimes, Disk, Gpu, Hardware, Memory, Volume};

pub fn hardware() -> Hardware {
    let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu = info
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| matches!(key.trim(), "model name" | "Hardware" | "Processor"))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_else(|| std::env::consts::ARCH.into());
    let mut cores = HashSet::new();
    let mut logical = 0;

    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if !name
                .strip_prefix("cpu")
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()))
            {
                continue;
            }

            let path = entry.path();

            if read_text(path.join("online")).as_deref() == Some("0") {
                continue;
            }

            logical += 1;

            if let (Some(package), Some(core)) = (
                read_text(path.join("topology/physical_package_id")),
                read_text(path.join("topology/core_id")),
            ) {
                cores.insert((package, core));
            }
        }
    }

    let mut notes = Vec::new();
    let gpus = gpus().unwrap_or_else(|| {
        notes.push("GPU enumeration unavailable".into());

        Vec::new()
    });
    let disks = disks().unwrap_or_else(|| {
        notes.push("Physical disk enumeration unavailable".into());

        Vec::new()
    });

    Hardware {
        cpu,
        physical_cores: (!cores.is_empty()).then_some(cores.len()),
        logical_cores: (logical > 0).then_some(logical),
        // MemTotal is usable RAM; firmware-reserved RAM is not included.
        installed_memory: None,
        gpus,
        disks,
        notes,
        ..Default::default()
    }
}

pub fn cpu_times() -> Option<CpuTimes> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut fields = line.split_whitespace();

    if fields.next()? != "cpu" {
        return None;
    }

    // Guest times are already included in user/nice.
    let times: Vec<u64> = fields
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;

    if times.len() < 4 {
        return None;
    }

    let total: u64 = times.iter().sum();
    let idle = times[3].saturating_add(times.get(4).copied().unwrap_or(0));

    Some(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

pub fn memory() -> Option<Memory> {
    let info = fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        info.lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;

                (key == name).then(|| value.split_whitespace().next()?.parse::<u64>().ok())?
            })
            .map(|kib| kib.saturating_mul(1024))
    };

    let total = field("MemTotal")?;
    let available = field("MemAvailable").or_else(|| {
        Some(
            field("MemFree")?
                .saturating_add(field("Buffers")?)
                .saturating_add(field("Cached")?),
        )
    })?;

    Some(Memory {
        total,
        available: available.min(total),
    })
}

fn disks() -> Option<Vec<Disk>> {
    let mut disks = Vec::new();

    for entry in fs::read_dir("/sys/class/block").ok()?.flatten() {
        let path = entry.path();

        if path.join("partition").exists() {
            continue;
        }

        let actual = fs::canonicalize(&path).ok();

        if actual
            .as_ref()
            .is_none_or(|path| path.starts_with("/sys/devices/virtual"))
        {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        let model = read_text(path.join("device/model"));
        let name = model.map_or_else(|| name.clone(), |model| format!("{name} · {model}"));
        let capacity = read_text(path.join("size"))
            .and_then(|size| size.parse::<u64>().ok())
            .and_then(|sectors| sectors.checked_mul(512));
        disks.push(Disk { name, capacity });
    }

    disks.sort_by(|a, b| a.name.cmp(&b.name));

    Some(disks)
}

fn gpus() -> Option<Vec<Gpu>> {
    let pci = fs::read_dir("/sys/bus/pci/devices");
    let drm = fs::read_dir("/sys/class/drm");

    if pci.is_err() && drm.is_err() {
        return None;
    }

    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    if let Ok(entries) = pci {
        for entry in entries.flatten() {
            let path = entry.path();

            if read_hex(path.join("class")).unwrap_or(0) >> 16 == 3
                && let Ok(path) = fs::canonicalize(path)
                && seen.insert(path.clone())
            {
                paths.push(path);
            }
        }
    }

    if let Ok(entries) = drm {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if !name
                .strip_prefix("card")
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()))
            {
                continue;
            }

            if let Ok(path) = fs::canonicalize(entry.path().join("device"))
                && seen.insert(path.clone())
            {
                paths.push(path);
            }
        }
    }

    paths.sort();

    Some(paths.iter().map(|path| gpu(path)).collect())
}

fn gpu(path: &Path) -> Gpu {
    let vendor = read_hex(path.join("vendor"));
    let device = read_hex(path.join("device"));
    let driver_path = fs::read_link(path.join("driver")).ok();
    let driver_name = driver_path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    let driver = driver_name.as_ref().map(|name| {
        let version = read_text(path.join("driver/module/version"))
            .or_else(|| read_text("/proc/sys/kernel/osrelease"));

        version.map_or_else(|| name.clone(), |version| format!("{name} {version}"))
    });
    let bus = path.file_name().unwrap_or_default().to_string_lossy();
    let nvidia_name =
        read_text(format!("/proc/driver/nvidia/gpus/{bus}/information")).and_then(|info| {
            info.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;

                (key.trim() == "Model").then(|| value.trim().to_owned())
            })
        });
    let name = nvidia_name
        .or_else(|| read_text(path.join("product_name")))
        .or_else(|| pci_name(vendor?, device?))
        .or_else(|| {
            read_text(path.join("of_node/compatible"))
                .map(|name| name.trim_end_matches('\0').replace('\0', " · "))
        })
        .unwrap_or_else(|| match (vendor, device) {
            (Some(vendor), Some(device)) => format!("GPU {vendor:04x}:{device:04x}"),
            _ => format!("GPU · {bus}"),
        });
    let memory = read_text(path.join("mem_info_vram_total"))
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0);
    let status = match driver_name.as_deref() {
        None => "No driver bound",
        Some("vfio-pci" | "pci-stub") => "Assigned to passthrough driver",
        Some(_) => "Driver bound",
    }
    .into();

    Gpu {
        name,
        driver,
        status,
        driver_available: Some(
            driver_name
                .as_deref()
                .is_some_and(|name| !matches!(name, "vfio-pci" | "pci-stub")),
        ),
        memory,
    }
}

fn pci_name(vendor: u32, device: u32) -> Option<String> {
    static IDS: LazyLock<String> = LazyLock::new(|| {
        [
            "/usr/share/hwdata/pci.ids",
            "/usr/share/misc/pci.ids",
            "/usr/share/pci.ids",
        ]
        .into_iter()
        .find_map(|path| fs::read_to_string(path).ok())
        .unwrap_or_default()
    });

    let vendor_id = format!("{vendor:04x}  ");
    let device_id = format!("\t{device:04x}  ");
    let mut vendor_name = None;

    for line in IDS.lines() {
        if let Some(name) = line.strip_prefix(&vendor_id) {
            vendor_name = Some(name.trim());
        } else if let Some(vendor_name) = vendor_name {
            if let Some(name) = line.strip_prefix(&device_id) {
                return Some(format!("{vendor_name} {}", name.trim()));
            }

            if !line.starts_with(['\t', '#']) && !line.is_empty() {
                break;
            }
        }
    }

    vendor_name.map(|name| format!("{name} · {device:04x}"))
}

pub fn volumes() -> Vec<Volume> {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut mounts = Vec::new();

    for line in info.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = before.split_whitespace().collect();
        let filesystem = after.split_whitespace().next().unwrap_or("");

        if fields.len() < 5 || !Path::new("/sys/dev/block").join(fields[2]).exists() {
            continue;
        }

        if matches!(filesystem, "nfs" | "nfs4" | "cifs" | "smb3" | "9p") {
            continue;
        }

        mounts.push((fields[2].to_owned(), unescape_mount(fields[4])));
    }

    mounts.sort_by_key(|(_, path)| path.len());
    let mut seen = HashSet::new();
    let mut volumes = Vec::new();

    for (device, path) in mounts {
        if !seen.insert(device) {
            continue;
        }

        let path = PathBuf::from(path);
        let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
            continue;
        };
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: terminated path and correctly sized output; read only on success.
        if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            continue;
        }

        // SAFETY: statvfs initialized the output on success.
        let stat = unsafe { stat.assume_init() };
        // libc's integer widths vary by target.
        #[allow(clippy::unnecessary_cast)]
        let (total, available) = (
            (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64),
            (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64),
        );

        if total > 0 {
            volumes.push(Volume {
                name: path.display().to_string(),
                total,
                available,
            });
        }
    }

    volumes.sort_by(|a, b| a.name.cmp(&b.name));

    volumes
}

fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn read_hex(path: impl AsRef<Path>) -> Option<u32> {
    let text = read_text(path)?;

    u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}
