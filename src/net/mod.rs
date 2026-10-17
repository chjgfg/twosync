//! 联网的两层（M1）。分工是硬的：**`frame` 只管"一条消息怎么变成字节"，`session` 只管
//! "一次会话里消息的先后"** —— 换传输（SSH → 别的管子）只动调用方，换协议只动这两处。
//!
//! [`frame`]：消息类型 + 4 字节长度前缀的 postcard 编解码 + `MAX_FRAME` 与
//! **"先查长度再分配"**（§9.4.5：长度是对端可控的，这条必须在分配之前）+ 块数据段的
//! `[hash][len][bytes]` 流式读写（发送前自证）+ `caps`/`proto_ver` 协商 + `check_project_id`。
//!
//! [`session`]：七步会话（§7），只要求对端是一根 [`session::Io`] = `Read + Write`。
//! 真实管子是 `ssh <peer> twosync agent` 的 stdio（§9.5，不开新端口、不引入异步运行时），
//! 测试里换成内存管道 ⇒ **同一份代码**，见 `tests/net_session.rs`。
//!
//! 还没做的那两格如实标在 §11.0：TOFU 指纹表（`peer add|list|trust`）与块级配额淘汰。

pub mod frame;
pub mod session;
