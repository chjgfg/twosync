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
