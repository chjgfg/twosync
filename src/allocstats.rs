//! 一个计数用的全局分配器，用来把 §9.6.1「扫描峰值新增分配 ≤ 4 MB」量成可断言的数字。
//!
//! 只实现 `alloc`/`dealloc` 就够：`GlobalAlloc` 的 `realloc`/`alloc_zeroed` 默认实现会
//! 走这两个方法，计数自动正确。单线程下峰值精确，多线程下是近似上界（够用，别当 profiler 用）。

use std::{
    alloc::{GlobalAlloc, System},
    sync::atomic::{AtomicUsize, Ordering},
};

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

pub struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// 一段测量的起点：把峰值水位抬到"当前已用"，之后只关心**增量**。
pub struct Watermark {
    base: usize,
}

pub fn watermark() -> Watermark {
    let current = CURRENT.load(Ordering::Relaxed);
    PEAK.store(current, Ordering::Relaxed);
    Watermark { base: current }
}

impl Watermark {
    /// 这段窗口里额外堆占用的峰值（字节）。
    pub fn peak_growth(&self) -> usize {
        PEAK.load(Ordering::Relaxed).saturating_sub(self.base)
    }
}

/// **进程自己的峰值 RSS**（字节），拿不到就 `None`。
///
/// 为什么不用堆计数当 RSS：`Counting` 只看得见**我们自己的 `GlobalAlloc` 走量**，而 §10.8 要的那一列
/// 是"这个引擎跑起来占多少内存"——引擎自己 mmap/读缓存的那部分根本不经过我们的分配器。
/// 两列都要，缺一列就是半张表。
///
/// - Windows：`K32GetProcessMemoryInfo` 的 `PeakWorkingSetSize`（进程一生 monotone ⇒ **一个进程只测一个引擎**，
///   这就是 `tests/engine_perf.rs` 按 `TWOSYNC_PERF_ENGINE` 一遍一遍跑的 raison d'être）。
/// - Linux：`/proc/self/status` 的 `VmHWM`（同样是高水位）。
#[derive(Debug, Clone, Copy)]
pub struct Rss {
    /// 峰值工作集（≈ 峰值 RSS）
    pub peak_bytes: u64,
    /// 当前工作集
    pub now_bytes: u64,
}

#[cfg(windows)]
mod imp {
    //! 手工声明这几个类型：**不为一个结构体把 `windows` crate 拉进依赖树**
    //! （NFR-5 的"引擎自身占多少字节"也包括二进制大小，多一个依赖就多一份）。
    //! 布局必须与 Windows SDK 的 `PROCESS_MEMORY_COUNTERS` 逐字段对齐——前两个是 `DWORD`，
    //! 后面全是 `SIZE_T`（这里用 `usize`，与指针同宽）。

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }

    pub fn sample() -> Option<super::Rss> {
        let mut c = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut c,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(super::Rss {
            peak_bytes: c.peak_working_set_size as u64,
            now_bytes: c.working_set_size as u64,
        })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn sample() -> Option<super::Rss> {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        let pick = |key: &str| -> Option<u64> {
            text.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k == key).then(|| {
                    v.trim()
                        .trim_end_matches(" kB")
                        .trim()
                        .parse::<u64>()
                        .map(|kb| kb * 1024)
                        .unwrap_or(0)
                })
            })
        };
        Some(super::Rss {
            peak_bytes: pick("VmHWM")?,
            now_bytes: pick("VmRSS")?,
        })
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn sample() -> Option<super::Rss> {
        None
    }
}

pub fn peak_rss() -> Option<Rss> {
    imp::sample()
}
