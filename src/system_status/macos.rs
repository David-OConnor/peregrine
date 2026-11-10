use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_int, c_void},
    mem::{MaybeUninit, size_of},
    ptr::{null, null_mut},
};

use super::{CpuTimes, Disk, Gpu, Hardware, Memory, Volume, processes::Reading};

pub const PROCESS_MEMORY: &str = "Physical footprint, as Activity Monitor shows";

/// Keeps each process's name, so it needs looking up once.
#[derive(Default)]
pub struct ProcessReader {
    pids: Vec<c_int>,
    names: HashMap<(u32, u64), String>,
}

impl ProcessReader {
    pub fn read(&mut self) -> Option<Vec<Reading>> {
        // SAFETY: a null buffer asks for how many there are.
        let estimate = unsafe { libc::proc_listallpids(null_mut(), 0) };

        // Some room for any started since.
        self.pids
            .resize(usize::try_from(estimate).ok().filter(|n| *n > 0)? + 32, 0);

        let bytes = c_int::try_from(self.pids.len() * size_of::<c_int>()).ok()?;
        // SAFETY: the length passed is the buffer's own.
        let count = unsafe { libc::proc_listallpids(self.pids.as_mut_ptr().cast(), bytes) };
        let count = usize::try_from(count).ok().filter(|n| *n > 0)?;
        let mut timebase = MaybeUninit::<libc::mach_timebase_info>::zeroed();
        // SAFETY: valid output pointer.
        if unsafe { libc::mach_timebase_info(timebase.as_mut_ptr()) } != 0 {
            return None;
        }

        // SAFETY: successful mach_timebase_info initialized the structure.
        let timebase = unsafe { timebase.assume_init() };

        if timebase.denom == 0 {
            return None;
        }

        let mut names = HashMap::with_capacity(self.names.len());
        let mut readings = Vec::with_capacity(count);

        for &pid in &self.pids[..count.min(self.pids.len())] {
            let mut info = MaybeUninit::<libc::rusage_info_v2>::zeroed();
            // Fails for other users' processes, without root.
            // SAFETY: the output's the type RUSAGE_INFO_V2 writes; libc
            // declares the pointer's type oddly, but it's only a buffer.
            if pid <= 0
                || unsafe {
                    libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast())
                } != 0
            {
                continue;
            }

            // SAFETY: successful proc_pid_rusage initialized the structure.
            let info = unsafe { info.assume_init() };
            let key = (pid as u32, info.ri_proc_start_abstime);
            let Some(name) = self.names.remove(&key).or_else(|| process_name(pid)) else {
                continue;
            };

            names.insert(key, name.clone());

            // In Mach time units, which aren't nanoseconds on Apple Silicon.
            let ticks = info.ri_user_time.saturating_add(info.ri_system_time);

            readings.push(Reading {
                pid: key.0,
                started: key.1,
                name,
                cpu_ns: ticks.saturating_mul(u64::from(timebase.numer)) / u64::from(timebase.denom),
                memory: info.ri_phys_footprint,
            });
        }

        self.names = names;

        Some(readings)
    }
}

fn process_name(pid: c_int) -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the length passed is the buffer's own.
    let length = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let length = usize::try_from(length).ok().filter(|n| *n > 0)?;

    Some(String::from_utf8_lossy(&buffer[..length.min(buffer.len())]).into_owned())
}

type Object = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: Object);
    fn CFGetTypeID(object: Object) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringCreateWithCString(allocator: Object, text: *const c_char, encoding: u32) -> Object;
    fn CFStringGetCString(string: Object, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
    fn CFArrayGetCount(array: Object) -> isize;
    fn CFArrayGetValueAtIndex(array: Object, index: isize) -> Object;
    fn CFBooleanGetValue(value: Object) -> bool;
    fn CFNumberGetValue(number: Object, kind: isize, value: *mut c_void) -> bool;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> Object;
    fn IOServiceGetMatchingServices(port: u32, matching: Object, iterator: *mut u32) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: Object,
        allocator: Object,
        options: u32,
    ) -> Object;
    fn IORegistryEntryGetName(entry: u32, name: *mut c_char) -> i32;
}

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    fn MTLCopyAllDevices() -> Object;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn sel_registerName(name: *const c_char) -> Object;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

struct CfObject(Object);

impl Drop for CfObject {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this wrapper owns a Create/Copy reference.
            unsafe { CFRelease(self.0) };
        }
    }
}

struct IoObject(u32);

impl Drop for IoObject {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the iterator or service reference.
        unsafe { IOObjectRelease(self.0) };
    }
}

struct HostPort(libc::mach_port_t);

impl HostPort {
    fn new() -> Self {
        // SAFETY: returns an owned send right to the current host.
        Self(unsafe { libc::mach_host_self() })
    }
}

impl Drop for HostPort {
    fn drop(&mut self) {
        // SAFETY: release the send right acquired by mach_host_self.
        unsafe { libc::mach_port_deallocate(libc::mach_task_self(), self.0) };
    }
}

pub fn hardware() -> Hardware {
    let mut notes = Vec::new();
    let disks = disks().unwrap_or_else(|| {
        notes.push("Physical disk enumeration unavailable".into());

        Vec::new()
    });

    Hardware {
        cpu: sysctl_string(c"machdep.cpu.brand_string")
            .unwrap_or_else(|| std::env::consts::ARCH.into()),
        physical_cores: sysctl_number(c"hw.physicalcpu").and_then(|n| usize::try_from(n).ok()),
        logical_cores: sysctl_number(c"hw.logicalcpu").and_then(|n| usize::try_from(n).ok()),
        installed_memory: sysctl_number(c"hw.memsize"),
        gpus: gpus(),
        disks,
        notes,
        ..Default::default()
    }
}

pub fn cpu_times() -> Option<CpuTimes> {
    let host = HostPort::new();
    let mut info = MaybeUninit::<libc::host_cpu_load_info>::zeroed();
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: typed output has the requested number of integer slots.
    if unsafe {
        libc::host_statistics(
            host.0,
            libc::HOST_CPU_LOAD_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    } != 0
    {
        return None;
    }

    // SAFETY: successful host_statistics initialized the structure.
    let info = unsafe { info.assume_init() };
    let total: u64 = info.cpu_ticks.iter().map(|&n| u64::from(n)).sum();
    let idle = u64::from(info.cpu_ticks[libc::CPU_STATE_IDLE as usize]);

    Some(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

pub fn memory() -> Option<Memory> {
    let total = sysctl_number(c"hw.memsize")?;
    let host = HostPort::new();
    let mut info = MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: typed output and its size in integer slots match HOST_VM_INFO64.
    if unsafe {
        libc::host_statistics64(
            host.0,
            libc::HOST_VM_INFO64,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    } != 0
    {
        return None;
    }

    // SAFETY: output was initialized; sysconf has no pointer arguments.
    let info = unsafe { info.assume_init() };
    let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let used_pages = u64::from(info.internal_page_count)
        .saturating_add(u64::from(info.wire_count))
        .saturating_add(u64::from(info.compressor_page_count))
        .saturating_sub(u64::from(info.purgeable_count));
    let available = total.saturating_sub(used_pages.saturating_mul(page_size));

    Some(Memory { total, available })
}

pub fn volumes() -> Vec<Volume> {
    // SAFETY: null buffer requests the count; NOWAIT uses cached filesystem stats.
    let count = unsafe { libc::getfsstat(null_mut(), 0, libc::MNT_NOWAIT) };

    if count <= 0 {
        return Vec::new();
    }

    let mut buffer: Vec<MaybeUninit<libc::statfs>> = Vec::with_capacity(count as usize + 8);
    let bytes = buffer.capacity() * size_of::<libc::statfs>();
    // SAFETY: allocated capacity covers `bytes`; only returned entries are read.
    let count =
        unsafe { libc::getfsstat(buffer.as_mut_ptr().cast(), bytes as i32, libc::MNT_NOWAIT) };

    if count <= 0 {
        return Vec::new();
    }

    let mut volumes = Vec::new();

    for index in 0..(count as usize).min(buffer.capacity()) {
        // SAFETY: index is below the returned, initialized entry count.
        let stat = unsafe { buffer.as_ptr().add(index).cast::<libc::statfs>().read() };

        if stat.f_flags & libc::MNT_LOCAL as u32 == 0 || stat.f_blocks == 0 {
            continue;
        }

        // SAFETY: statfs returns terminated mount paths in these arrays.
        let path = unsafe { CStr::from_ptr(stat.f_mntonname.as_ptr()) }
            .to_string_lossy()
            .into_owned();

        if path.starts_with("/System/Volumes/") && path != "/System/Volumes/Data" {
            continue;
        }

        volumes.push(Volume {
            name: path,
            total: stat.f_blocks.saturating_mul(u64::from(stat.f_bsize)),
            available: stat.f_bavail.saturating_mul(u64::from(stat.f_bsize)),
        });
    }

    volumes
}

fn disks() -> Option<Vec<Disk>> {
    let mut iterator = 0;
    // SAFETY: IOServiceGetMatchingServices consumes the matching dictionary.
    let result = unsafe {
        IOServiceGetMatchingServices(0, IOServiceMatching(c"IOMedia".as_ptr()), &mut iterator)
    };

    if result != 0 {
        return None;
    }

    let iterator = IoObject(iterator);
    let mut disks = Vec::new();

    loop {
        // SAFETY: live iterator; each returned service is owned.
        let service = unsafe { IOIteratorNext(iterator.0) };

        if service == 0 {
            break;
        }

        let service = IoObject(service);
        let whole = property(service.0, c"Whole");

        // SAFETY: check the CoreFoundation type before reading its value.
        if whole.0.is_null()
            || unsafe {
                CFGetTypeID(whole.0) != CFBooleanGetTypeID() || !CFBooleanGetValue(whole.0)
            }
        {
            continue;
        }

        let size = property(service.0, c"Size");
        let mut capacity = 0i64;
        // SAFETY: type checked; kind 4 is kCFNumberSInt64Type with an i64 output.
        let has_size = !size.0.is_null()
            && unsafe {
                CFGetTypeID(size.0) == CFNumberGetTypeID()
                    && CFNumberGetValue(size.0, 4, (&mut capacity as *mut i64).cast())
            };
        let mut name = [0 as c_char; 128];
        // SAFETY: io_name_t is a 128-byte, terminated character array.
        let named = unsafe { IORegistryEntryGetName(service.0, name.as_mut_ptr()) } == 0;
        let name = if named {
            // SAFETY: successful name query terminated the buffer.
            unsafe { CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            "Disk".into()
        };
        disks.push(Disk {
            name,
            capacity: has_size
                .then_some(capacity)
                .and_then(|n| u64::try_from(n).ok()),
        });
    }

    Some(disks)
}

fn property(service: u32, name: &CStr) -> CfObject {
    // SAFETY: UTF-8 key is terminated; Create results are released by wrappers.
    let key = CfObject(unsafe { CFStringCreateWithCString(null(), name.as_ptr(), 0x08000100) });

    if key.0.is_null() {
        return CfObject(null());
    }

    CfObject(unsafe { IORegistryEntryCreateCFProperty(service, key.0, null(), 0) })
}

fn gpus() -> Vec<Gpu> {
    // SAFETY: pool bounds autoreleased objects from Metal/Objective-C calls.
    let pool = unsafe { objc_autoreleasePoolPush() };
    let devices = CfObject(unsafe { MTLCopyAllDevices() });
    let os_version = sysctl_string(c"kern.osproductversion");
    let mut gpus = Vec::new();

    if !devices.0.is_null() {
        // SAFETY: MTLCopyAllDevices returns an NSArray/CFArray of MTLDevice objects.
        let count = unsafe { CFArrayGetCount(devices.0) };
        // SAFETY: typed message signature matches -[MTLDevice name].
        let name_message: unsafe extern "C" fn(Object, Object) -> Object =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        let selector = unsafe { sel_registerName(c"name".as_ptr()) };

        for index in 0..count {
            // SAFETY: index is within the array; name is a borrowed NSString/CFString.
            let name = unsafe {
                let device = CFArrayGetValueAtIndex(devices.0, index);
                cf_string(name_message(device, selector))
            }
            .unwrap_or_else(|| "Metal GPU".into());
            let driver = Some(os_version.as_ref().map_or_else(
                || "Metal (system driver)".into(),
                |version| format!("Metal · macOS {version}"),
            ));
            gpus.push(Gpu {
                name,
                driver,
                status: "Metal available".into(),
                driver_available: Some(true),
                memory: None,
            });
        }
    }

    drop(devices);
    // SAFETY: releases the pool created above after borrowed names were copied.
    unsafe { objc_autoreleasePoolPop(pool) };

    gpus
}

unsafe fn cf_string(value: Object) -> Option<String> {
    // SAFETY: caller supplies a valid borrowed CF object; verify its type.
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return None;
    }

    let mut buffer = [0 as c_char; 1024];
    // SAFETY: typed CFString and output buffer length match.
    if !unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
            0x08000100,
        )
    } {
        return None;
    }

    Some(
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buffer = [0u8; 1024];
    let mut bytes = buffer.len();
    // SAFETY: terminated key, matching output buffer; no new value is supplied.
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
            null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }

    Some(
        String::from_utf8_lossy(&buffer[..bytes.min(buffer.len())])
            .trim_end_matches('\0')
            .trim()
            .to_owned(),
    )
}

fn sysctl_number(name: &CStr) -> Option<u64> {
    let mut buffer = [0u8; 8];
    let mut bytes = buffer.len();
    // SAFETY: terminated key, eight-byte output buffer; read-only query.
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
            null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }

    match bytes {
        4 => Some(u64::from(u32::from_ne_bytes(buffer[..4].try_into().ok()?))),
        8 => Some(u64::from_ne_bytes(buffer)),
        _ => None,
    }
}
