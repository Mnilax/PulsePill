//! PC vitals: CPU, GPU, memory, network, battery, disk, uptime. Plain Win32 calls, ~1 Hz.

use crate::model::{current_tile, SystemSnapshot, Tile};
use std::collections::HashMap;
use std::time::Instant;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::SystemInformation::{GetPhysicallyInstalledSystemMemory, GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetSystemTimes};

const HISTORY: usize = 60;

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

struct Gpu {
    query: isize,
    counter: isize,
    buf: Vec<u8>,
}

impl Gpu {
    fn new() -> Option<Gpu> {
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            let path: Vec<u16> = "\\GPU Engine(*)\\Utilization Percentage\0".encode_utf16().collect();
            let mut counter = 0isize;
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            PdhCollectQueryData(query);
            Some(Gpu { query, counter, buf: Vec::new() })
        }
    }

    /// Task-Manager-style overall GPU load: the busiest engine type (3D, Copy, VideoDecode…).
    fn sample(&mut self) -> Option<f64> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let rc = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut());
            if rc as u32 != PDH_MORE_DATA as u32 || size == 0 {
                return Some(0.0);
            }
            self.buf.resize(size as usize, 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, items) != 0 {
                return None;
            }
            let mut by_type: HashMap<String, f64> = HashMap::new();
            for i in 0..count as usize {
                let item = &*items.add(i);
                if item.FmtValue.CStatus != 0 {
                    continue;
                }
                let mut len = 0;
                while *item.szName.add(len) != 0 {
                    len += 1;
                }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(item.szName, len));
                let ty = name.rsplit("engtype_").next().unwrap_or("").to_string();
                *by_type.entry(ty).or_default() += item.FmtValue.Anonymous.doubleValue;
            }
            // Keep the scratch buffer from growing permanently after a spike in instances.
            if self.buf.capacity() > 256 * 1024 {
                self.buf = Vec::new();
            }
            Some(by_type.values().fold(0.0f64, |a, &b| a.max(b)).clamp(0.0, 100.0) / 100.0)
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// Physical-disk activity, Task Manager style ("Active time" + read/write speed). Opened only
/// while the disk-activity tile is selected, since PDH costs a few MB.
struct Disk {
    query: isize,
    idle: isize,
    read: isize,
    write: isize,
}

impl Disk {
    fn new() -> Option<Disk> {
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            let add = |path: &str| -> Option<isize> {
                let p: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
                let mut c = 0isize;
                (PdhAddEnglishCounterW(query, p.as_ptr(), 0, &mut c) == 0).then_some(c)
            };
            let counters = (
                add("\\PhysicalDisk(_Total)\\% Idle Time"),
                add("\\PhysicalDisk(_Total)\\Disk Read Bytes/sec"),
                add("\\PhysicalDisk(_Total)\\Disk Write Bytes/sec"),
            );
            let (Some(idle), Some(read), Some(write)) = counters else {
                PdhCloseQuery(query);
                return None;
            };
            PdhCollectQueryData(query);
            Some(Disk { query, idle, read, write })
        }
    }

    fn value(counter: isize) -> Option<f64> {
        unsafe {
            let mut v: PDH_FMT_COUNTERVALUE = std::mem::zeroed();
            let ok = PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE | 0x8000 /* NOCAP100 */, std::ptr::null_mut(), &mut v) == 0 && v.CStatus == 0;
            ok.then(|| v.Anonymous.doubleValue)
        }
    }

    /// (active fraction, read B/s, write B/s)
    fn sample(&mut self) -> Option<(f64, f64, f64)> {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return None;
        }
        let idle = Disk::value(self.idle)?;
        Some((((100.0 - idle) / 100.0).clamp(0.0, 1.0), Disk::value(self.read).unwrap_or(0.0), Disk::value(self.write).unwrap_or(0.0)))
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

pub struct Sampler {
    prev_cpu: Option<(u64, u64)>,
    prev_net: Option<(u64, u64, Instant)>,
    gpu: Option<Gpu>,
    disk: Option<Disk>,
    ticks: u64,
    snap: SystemSnapshot,
}

impl Sampler {
    pub fn new(gpu: bool) -> Sampler {
        let drive = std::env::var("SystemDrive").ok().filter(|d| d.len() == 2).unwrap_or_else(|| "C:".into());
        let snap = SystemSnapshot { disk_drive: drive, ..Default::default() };
        Sampler { prev_cpu: None, prev_net: None, gpu: if gpu { Gpu::new() } else { None }, disk: None, ticks: 0, snap }
    }

    pub fn sample(&mut self) -> SystemSnapshot {
        self.cpu();
        self.memory();
        self.network();
        self.battery();
        if self.ticks % 10 == 0 {
            self.disk_space();
        }
        self.disk_activity();
        self.snap.uptime_secs = unsafe { GetTickCount64() } / 1000;
        self.ticks += 1;
        if let Some(gpu) = self.gpu.as_mut() {
            if let Some(v) = gpu.sample() {
                self.snap.gpu_usage = Some(v);
                push(&mut self.snap.gpu_history, v);
            }
        }
        // Private bytes: close to Task Manager's "Memory" column. The full working set would
        // also count pages of system DLLs shared with every other process.
        self.snap.self_memory = unsafe {
            let mut pmc: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, pmc.cb);
            pmc.PrivateUsage as u64
        };
        self.snap.clone()
    }

    fn cpu(&mut self) {
        let (mut idle, mut kernel, mut user) = unsafe { std::mem::zeroed::<(FILETIME, FILETIME, FILETIME)>() };
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return;
        }
        let (idle, total) = (ft(idle), ft(kernel) + ft(user)); // kernel time includes idle time
        if let Some((pi, pt)) = self.prev_cpu {
            let dt = total.saturating_sub(pt);
            if dt > 0 {
                let busy = 1.0 - idle.saturating_sub(pi) as f64 / dt as f64;
                self.snap.cpu_usage = busy.clamp(0.0, 1.0);
                push(&mut self.snap.cpu_history, self.snap.cpu_usage);
            }
        }
        self.prev_cpu = Some((idle, total));
    }

    fn memory(&mut self) {
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 {
            // Show installed RAM ("32 GB"), like Task Manager; ullTotalPhys excludes memory
            // reserved by firmware and integrated graphics ("31 GB").
            let mut installed_kb = 0u64;
            let installed = unsafe { GetPhysicallyInstalledSystemMemory(&mut installed_kb) } != 0;
            self.snap.memory_total = if installed && installed_kb * 1024 >= m.ullTotalPhys { installed_kb * 1024 } else { m.ullTotalPhys };
            self.snap.memory_used = m.ullTotalPhys - m.ullAvailPhys;
            self.snap.memory_load = m.dwMemoryLoad as f64 / 100.0;
        }
    }

    fn network(&mut self) {
        let (mut down, mut up) = (0u64, 0u64);
        unsafe {
            let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table) != 0 || table.is_null() {
                return;
            }
            let n = (*table).NumEntries as usize;
            let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
            for row in rows {
                let flags = row.InterfaceAndOperStatusFlags._bitfield;
                let hardware = flags & 0x1 != 0;
                let filter = flags & 0x2 != 0;
                // Physical, non-filter, up, not loopback: avoids double counting WFP/QoS layers.
                if hardware && !filter && row.OperStatus == 1 && row.Type != 24 {
                    down += row.InOctets;
                    up += row.OutOctets;
                }
            }
            FreeMibTable(table as *const _);
        }
        let now = Instant::now();
        if let Some((pd, pu, at)) = self.prev_net {
            let dt = now.duration_since(at).as_secs_f64().max(0.001);
            self.snap.net_down = down.saturating_sub(pd) as f64 / dt;
            self.snap.net_up = up.saturating_sub(pu) as f64 / dt;
        }
        self.prev_net = Some((down, up, now));
    }

    fn battery(&mut self) {
        let mut p: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        if unsafe { GetSystemPowerStatus(&mut p) } == 0 {
            return;
        }
        let no_battery = p.BatteryFlag & 128 != 0 || p.BatteryFlag == 255 || p.BatteryLifePercent > 100;
        self.snap.battery_level = if no_battery { None } else { Some(p.BatteryLifePercent as f64 / 100.0) };
        self.snap.is_plugged_in = p.ACLineStatus == 1;
        self.snap.is_charging = p.BatteryFlag & 8 != 0;
        self.snap.battery_minutes = if p.BatteryLifeTime == u32::MAX || self.snap.is_plugged_in { None } else { Some(p.BatteryLifeTime / 60) };
    }
}

impl Sampler {
    fn disk_space(&mut self) {
        let root: Vec<u16> = format!("{}\\", self.snap.disk_drive).encode_utf16().chain(Some(0)).collect();
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), std::ptr::null_mut(), &mut total, &mut free) } != 0 {
            self.snap.disk_total = total;
            self.snap.disk_used = total.saturating_sub(free);
        }
    }

    fn disk_activity(&mut self) {
        let wanted = current_tile() == Tile::DiskActivity;
        if !wanted {
            if self.disk.take().is_some() {
                self.snap.disk_active = None;
                self.snap.disk_active_history.clear();
            }
            return;
        }
        if self.disk.is_none() {
            self.disk = Disk::new();
        }
        if let Some((active, read, write)) = self.disk.as_mut().and_then(Disk::sample) {
            self.snap.disk_active = Some(active);
            self.snap.disk_read = read;
            self.snap.disk_write = write;
            push(&mut self.snap.disk_active_history, active);
        }
    }
}

fn push(h: &mut Vec<f64>, v: f64) {
    if h.len() >= HISTORY {
        h.remove(0);
    }
    h.push(v);
}
