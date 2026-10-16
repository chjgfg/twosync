//! 存储层：键编码 + 工作树扫描 + 对象读写。命令层不许直接拼键，一律走这里。

pub mod cas;
pub mod keys;
pub mod worktree;
