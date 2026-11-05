use std::{
    mem::size_of,
    path::PathBuf,
    ptr::{null, null_mut},
};

use windows_sys::{
    Win32::{
        Devices::DeviceAndDriverInstallation::*,
        Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, GetDriveTypeW,
            GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
        },
        System::{
            Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetThreadErrorMode},
            IO::DeviceIoControl,
            Ioctl::{DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX},
            Registry::{
                HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_QWORD, RRF_RT_REG_SZ,
                RegGetValueW,
            },
            SystemInformation::{
                GROUP_AFFINITY, GetLogicalProcessorInformationEx,
                GetPhysicallyInstalledSystemMemory, GlobalMemoryStatusEx, MEMORYSTATUSEX,
                RelationProcessorCore,
            },
            Threading::{
                ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetActiveProcessorGroupCount,
                GetCurrentThread, GetSystemTimes, GetThreadGroupAffinity, SetThreadGroupAffinity,
            },
            WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE},
        },
    },
    core::GUID,
};

use super::{CpuTimes, Disk, Gpu, Hardware, Memory, Volume};

const DISK_INTERFACE: GUID = GUID::from_u128(0x53f56307_b6bf_11d0_94f2_00a0c91efb8b);

pub fn hardware() -> Hardware {
    let cpu_key = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
    let mut installed_kib = 0;
    // SAFETY: valid output pointer; no ownership transfer.
    let installed = unsafe { GetPhysicallyInstalledSystemMemory(&mut installed_kib) } != 0;
    let mut notes = Vec::new();
    let gpus = gpus().unwrap_or_else(|| {
        notes.push("GPU enumeration unavailable".into());

        Vec::new()
    });
    let disks = disks().unwrap_or_else(|| {
        notes.push("Physical disk enumeration unavailable".into());

        Vec::new()
    });
    // SAFETY: ALL_PROCESSOR_GROUPS requests the system-wide count.
    let logical = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;

    Hardware {
        cpu: registry_string(HKEY_LOCAL_MACHINE, cpu_key, "ProcessorNameString")
            .unwrap_or_else(|| std::env::consts::ARCH.into()),
        physical_cores: physical_cores(),
        logical_cores: (logical > 0).then_some(logical),
        installed_memory: installed.then(|| installed_kib.saturating_mul(1024)),
        gpus,
        disks,
        notes,
        ..Default::default()
    }
}

fn physical_cores() -> Option<usize> {
    let mut bytes = 0;
    // SAFETY: the first call obtains the required buffer size.
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, null_mut(), &mut bytes) };

    if bytes == 0 {
        return None;
    }

    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
    // SAFETY: buffer is aligned and contains at least `bytes` writable bytes.
    if unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    } == 0
    {
        return None;
    }

    let mut offset = 0;
    let mut count = 0;

    while offset + 8 <= bytes as usize {
        // SAFETY: each record starts with two DWORDs; bounds checked above.
        let header = unsafe {
            buffer
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<[u32; 2]>()
                .read_unaligned()
        };
        let size = header[1] as usize;

        if size < 8 || offset + size > bytes as usize {
            return None;
        }

        count += usize::from(header[0] == RelationProcessorCore as u32);
        offset += size;
    }

    (count > 0).then_some(count)
}

fn read_cpu_times() -> Option<CpuTimes> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    // SAFETY: all three output pointers are valid.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }

    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let total = ticks(kernel).saturating_add(ticks(user));

    Some(CpuTimes {
        busy: total.saturating_sub(ticks(idle)),
        total,
    })
}

pub fn cpu_times() -> Option<CpuTimes> {
    // GetSystemTimes covers only the calling thread's processor group.
    // SAFETY: no arguments; returns the current system's group count.
    let groups = unsafe { GetActiveProcessorGroupCount() };

    if groups <= 1 {
        return read_cpu_times();
    }

    struct RestoreAffinity(GROUP_AFFINITY);

    impl Drop for RestoreAffinity {
        fn drop(&mut self) {
            // SAFETY: restores this worker thread's original affinity.
            unsafe { SetThreadGroupAffinity(GetCurrentThread(), &self.0, null_mut()) };
        }
    }

    let mut original = GROUP_AFFINITY::default();
    // SAFETY: valid output pointer and the current thread's pseudo-handle.
    if unsafe { GetThreadGroupAffinity(GetCurrentThread(), &mut original) } == 0 {
        return None;
    }

    let _restore = RestoreAffinity(original);
    let mut total = CpuTimes { busy: 0, total: 0 };

    for group in 0..groups {
        let affinity = GROUP_AFFINITY {
            Mask: 1,
            Group: group,
            ..Default::default()
        };
        // SAFETY: each active group contains processor zero; affects only this worker.
        if unsafe { SetThreadGroupAffinity(GetCurrentThread(), &affinity, null_mut()) } == 0 {
            return None;
        }

        let times = read_cpu_times()?;
        total.busy = total.busy.saturating_add(times.busy);
        total.total = total.total.saturating_add(times.total);
    }

    Some(total)
}

pub fn memory() -> Option<Memory> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: size and output pointer match MEMORYSTATUSEX.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;

    ok.then_some(Memory {
        total: status.ullTotalPhys,
        available: status.ullAvailPhys,
    })
}

struct Devices(HDEVINFO);

impl Devices {
    fn new(class: &GUID, flags: u32) -> Option<Self> {
        // SAFETY: valid class GUID; optional enumerator and parent are null.
        let handle =
            unsafe { SetupDiGetClassDevsW(class, null(), null_mut(), DIGCF_PRESENT | flags) };

        (handle != -1).then_some(Self(handle))
    }

    fn property(&self, device: &SP_DEVINFO_DATA, property: u32) -> Option<String> {
        let mut buffer = [0u16; 1024];
        // SAFETY: the buffer's byte length is passed and device belongs to this set.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                self.0,
                device,
                property,
                null_mut(),
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer) as u32,
                null_mut(),
            )
        } != 0;

        ok.then(|| wide_string(&buffer))
            .filter(|value| !value.is_empty())
    }

    fn name(&self, device: &SP_DEVINFO_DATA) -> String {
        self.property(device, SPDRP_FRIENDLYNAME)
            .or_else(|| self.property(device, SPDRP_DEVICEDESC))
            .unwrap_or_else(|| "Unknown device".into())
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        // SAFETY: this set is owned by this wrapper.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn gpus() -> Option<Vec<Gpu>> {
    let devices = Devices::new(&GUID_DEVCLASS_DISPLAY, 0)?;
    let mut gpus = Vec::new();

    for index in 0.. {
        let mut device = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: initialized output structure and a live device set.
        if unsafe { SetupDiEnumDeviceInfo(devices.0, index, &mut device) } == 0 {
            break;
        }

        let driver_key = devices
            .property(&device, SPDRP_DRIVER)
            .map(|key| format!(r"SYSTEM\CurrentControlSet\Control\Class\{key}"));
        let driver = driver_key
            .as_deref()
            .and_then(|key| registry_string(HKEY_LOCAL_MACHINE, key, "DriverVersion"));
        let memory = driver_key
            .as_deref()
            .and_then(|key| registry_qword(key, "HardwareInformation.qwMemorySize"));
        let mut flags = 0;
        let mut problem = 0;
        // SAFETY: valid device instance and output pointers.
        let result = unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, device.DevInst, 0) };
        let status = if result != CR_SUCCESS {
            "Driver status unavailable".into()
        } else if flags & DN_HAS_PROBLEM != 0 {
            match problem {
                22 => "Disabled".into(),
                28 => "Driver missing".into(),
                code => format!("Device problem (code {code})"),
            }
        } else if flags & DN_STARTED != 0 {
            "Driver running".into()
        } else {
            "Device not started".into()
        };
        gpus.push(Gpu {
            name: devices.name(&device),
            driver,
            status,
            driver_available: (result == CR_SUCCESS)
                .then_some(flags & DN_STARTED != 0 && flags & DN_HAS_PROBLEM == 0),
            memory,
        });
    }

    Some(gpus)
}

fn disks() -> Option<Vec<Disk>> {
    let devices = Devices::new(&DISK_INTERFACE, DIGCF_DEVICEINTERFACE)?;
    let mut disks = Vec::new();

    for index in 0.. {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: valid set, GUID, and initialized output structure.
        if unsafe {
            SetupDiEnumDeviceInterfaces(devices.0, null(), &DISK_INTERFACE, index, &mut interface)
        } == 0
        {
            break;
        }

        let mut bytes = 0;
        // SAFETY: sizing call; no output buffer yet.
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                devices.0,
                &interface,
                null_mut(),
                0,
                &mut bytes,
                null_mut(),
            )
        };

        if bytes < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
            continue;
        }

        let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
        let detail = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        let mut device = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: aligned buffer has the requested size; cbSize describes its header.
        let ok = unsafe {
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(
                devices.0,
                &interface,
                detail,
                bytes,
                null_mut(),
                &mut device,
            )
        } != 0;

        if !ok {
            continue;
        }

        // SAFETY: successful detail query returns a terminated device path.
        let capacity = unsafe { disk_capacity(std::ptr::addr_of!((*detail).DevicePath).cast()) };
        disks.push(Disk {
            name: devices.name(&device),
            capacity,
        });
    }

    Some(disks)
}

unsafe fn disk_capacity(path: *const u16) -> Option<u64> {
    // SAFETY: caller supplies a terminated path. Zero access permits metadata queries.
    let handle = unsafe {
        CreateFileW(
            path,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut buffer = [0u64; 128];
    let mut bytes = 0;
    // SAFETY: live handle and correctly sized/aligned output buffer; always closed.
    let ok = unsafe {
        let ok = DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            null(),
            0,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
            &mut bytes,
            null_mut(),
        );
        CloseHandle(handle);
        ok != 0
    };

    if !ok || (bytes as usize) < std::mem::offset_of!(DISK_GEOMETRY_EX, Data) {
        return None;
    }

    // SAFETY: output contains the complete DISK_GEOMETRY_EX header.
    let size = unsafe { (*buffer.as_ptr().cast::<DISK_GEOMETRY_EX>()).DiskSize };

    u64::try_from(size).ok().filter(|size| *size > 0)
}

pub fn volumes() -> Vec<Volume> {
    let mut volumes = Vec::new();
    let mut old_mode = 0;
    // SAFETY: worker-local error mode suppresses empty-card-reader dialogs.
    unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, &mut old_mode) };
    // SAFETY: returns the assigned drive-letter bitmask.
    let drives = unsafe { GetLogicalDrives() };

    for index in 0..26u8 {
        if drives & (1 << index) == 0 {
            continue;
        }

        let root = format!("{}:\\", char::from(b'A' + index));
        let wide = to_wide(&root);
        // SAFETY: root is terminated and lives through these calls.
        let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };

        if !matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE) {
            continue;
        }

        let mut available = 0;
        let mut total = 0;
        // SAFETY: valid root and output pointers; skips network and optical drives.
        if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, null_mut()) }
            == 0
        {
            continue;
        }

        let mut label = [0u16; 261];
        // SAFETY: output buffer's length matches; unused outputs are optional.
        unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            )
        };
        let label = wide_string(&label);
        let name = if label.is_empty() {
            root
        } else {
            format!("{root} · {label}")
        };
        volumes.push(Volume {
            name,
            total,
            available,
        });
    }

    // SAFETY: restores this worker's error mode.
    unsafe { SetThreadErrorMode(old_mode, null_mut()) };

    volumes
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn wide_string(buffer: &[u16]) -> String {
    let length = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());

    String::from_utf16_lossy(&buffer[..length])
        .trim()
        .to_owned()
}

/// Steam's install folder, as Steam records it.
pub fn steam_root() -> Option<PathBuf> {
    registry_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
        .or_else(|| {
            registry_string(
                HKEY_LOCAL_MACHINE,
                r"SOFTWARE\WOW6432Node\Valve\Steam",
                "InstallPath",
            )
        })
        .map(PathBuf::from)
}

fn registry_string(root: HKEY, key: &str, name: &str) -> Option<String> {
    let key = to_wide(key);
    let name = to_wide(name);
    let mut buffer = [0u16; 1024];
    let mut bytes = size_of_val(&buffer) as u32;
    // SAFETY: terminated names, typed output buffer, and matching byte count.
    let result = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };

    (result == 0)
        .then(|| wide_string(&buffer))
        .filter(|value| !value.is_empty())
}

fn registry_qword(key: &str, name: &str) -> Option<u64> {
    let key = to_wide(key);
    let name = to_wide(name);
    let mut value = 0u64;
    let mut bytes = size_of_val(&value) as u32;
    // SAFETY: terminated names and a QWORD-sized output buffer.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_QWORD,
            null_mut(),
            (&mut value as *mut u64).cast(),
            &mut bytes,
        )
    };

    (result == 0 && value > 0).then_some(value)
}
