//! 内容寻址块仓的四条规矩（§9.4.7 / §8 稀疏 / FR-5 增量 / FR-13 写序）。
//!
//! 这个文件里的 `stored` / `reused` 计数就是 §10.4 那张差分表的原材料：
//! "改 1 MB 只写 1 MB"不是一句口号，是 `one_byte_change_stores_exactly_one_block`。

use std::fs;
use std::path::Path;

use twosync::store::cas::{Cas, DEFAULT_BLOCK};

fn cas(dir: &Path) -> Cas {
    Cas::new(&dir.join("objects"), DEFAULT_BLOCK).expect("建块仓")
}

fn tmpdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// 每 64 KiB 打一个递增序号前缀 ⇒ 任意两个 4 MiB 块内容都不同。
/// 不用 `(i as u8)` 之类的模式：周期若能整除块大小，块与块就完全相同，
/// CAS 会把它们去重成一块，测试断言就会莫名其妙地少算。
fn distinct_body(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for (n, chunk) in v.chunks_mut(65536).enumerate() {
        chunk[..4].copy_from_slice(&(n as u32).to_be_bytes());
        for (m, b) in chunk.iter_mut().enumerate().skip(4) {
            *b = (m % 251) as u8;
        }
    }
    v
}

/// 数一下库里有多少个块文件（跳过 `.tmp.` 前缀与两级目录结构本身）。
fn count_blocks(objects: &Path) -> usize {
    if !objects.exists() {
        return 0;
    }
    fs::read_dir(objects)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .flat_map(|e| fs::read_dir(e.path()).unwrap().flatten())
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && !e.file_name().to_string_lossy().starts_with(".tmp.")
        })
        .count()
}

#[test]
fn roundtrip_rebuilds_the_exact_bytes() {
    let site = tmpdir();
    let store = cas(site.path());
    let src = site.path().join("src.bin");
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(&src, &body).unwrap();

    let out = store.put_file(&src).unwrap();
    assert_eq!(out.blob.size, body.len() as u64);
    assert_eq!(out.stored, 1, "200 KB 不足一块");

    let back = site.path().join("out.bin");
    store.read_blob(&out.blob, &back).unwrap();
    assert_eq!(fs::read(&back).unwrap(), body);
}

#[test]
fn second_put_reuses_everything_and_stores_nothing() {
    let site = tmpdir();
    let store = cas(site.path());
    let src = site.path().join("src.bin");
    fs::write(&src, distinct_body((DEFAULT_BLOCK * 2 + 11) as usize)).unwrap();

    let first = store.put_file(&src).unwrap();
    assert_eq!(first.stored, 3);
    assert_eq!(first.reused, 0);

    let second = store.put_file(&src).unwrap();
    assert_eq!(second.stored, 0, "同一份内容第二次不该再写");
    assert_eq!(second.reused, 3);
    assert_eq!(second.blob.hash, first.blob.hash, "整文件 hash 必须稳定");
    assert_eq!(count_blocks(&site.path().join("objects")), 3);
}

/// 规矩 2：全零块不落盘。10 MiB 全零文件应当一个块文件都不产生。
#[test]
fn zero_blocks_take_no_disk_space() {
    let site = tmpdir();
    let store = cas(site.path());
    let src = site.path().join("sparse.bin");
    fs::write(&src, vec![0u8; (DEFAULT_BLOCK * 2 + 4096) as usize]).unwrap();

    let out = store.put_file(&src).unwrap();
    assert_eq!(out.zero, 3, "两个整块 + 一个 4 KiB 尾块");
    assert_eq!(out.stored, 0);
    assert_eq!(
        count_blocks(&site.path().join("objects")),
        0,
        "全零块不许占磁盘"
    );

    let back = site.path().join("out.bin");
    store.read_blob(&out.blob, &back).unwrap();
    assert_eq!(
        fs::read(&back).unwrap().len(),
        (DEFAULT_BLOCK * 2 + 4096) as usize
    );
}

/// FR-5 的本地版：改 1 字节只写 1 块。这是"对 git 唯一领先点"的可执行定义。
#[test]
fn one_byte_change_stores_exactly_one_block() {
    let site = tmpdir();
    let store = cas(site.path());
    let src = site.path().join("db.sqlite");
    fs::write(&src, distinct_body((DEFAULT_BLOCK * 4) as usize)).unwrap();
    let before = store.put_file(&src).unwrap();
    assert_eq!(before.stored, 4);

    // 只动第 3 块里的一个字节。
    let mut body = fs::read(&src).unwrap();
    body[(DEFAULT_BLOCK * 2 + 7) as usize] = 9;
    fs::write(&src, &body).unwrap();

    let after = store.put_file(&src).unwrap();
    assert_eq!(
        after.stored, 1,
        "改 1 字节只该写 1 块，实际 {}",
        after.stored
    );
    assert_eq!(after.reused, 3);
    assert_eq!(after.stored_bytes, DEFAULT_BLOCK, "写的是整块，不是 1 字节");
}

/// 文件内部重复的块也只存一份：8 MiB 的重复 pattern 只占 4 MiB + 尾块。
/// 清单仍然描述全部 8 MiB，重建必须一模一样。
#[test]
fn duplicate_blocks_within_one_file_are_stored_once() {
    let site = tmpdir();
    let store = cas(site.path());
    let src = site.path().join("repeat.bin");
    let one = vec![42u8; DEFAULT_BLOCK as usize];
    fs::write(&src, [&one[..], &one[..]].concat()).unwrap();

    let out = store.put_file(&src).unwrap();
    assert_eq!(out.blob.blocks.len(), 2, "清单仍该描述两块");
    assert_eq!(
        out.blob.blocks[0].hash, out.blob.blocks[1].hash,
        "两块内容相同"
    );
    assert_eq!(out.stored, 1, "相同的块只落一份盘");
    assert_eq!(out.reused, 1);

    let back = site.path().join("out.bin");
    store.read_blob(&out.blob, &back).unwrap();
    assert_eq!(fs::read(&back).unwrap(), [&one[..], &one[..]].concat());
}

/// 规矩 1：返回之后库里不该留任何 .tmp。
#[test]
fn put_leaves_no_temp_files() {
    let site = tmpdir();
    let store = cas(site.path());
    let src = site.path().join("src.bin");
    fs::write(&src, vec![5u8; (DEFAULT_BLOCK + 7) as usize]).unwrap();
    store.put_file(&src).unwrap();

    let left: Vec<_> = fs::read_dir(site.path().join("objects"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tmp."))
        .collect();
    assert!(left.is_empty(), "残留 tmp：{left:?}");
}

/// 规矩 3：块被改坏一个字节，必须报错，**不许静默返回错内容**。
/// 这条直接对着引擎画像里 `blob_checksum=false` 那个缺口（§9.6.3）。
#[test]
fn corrupted_block_is_reported_not_silently_returned() {
    let site = tmpdir();
    let store = cas(site.path());
    let src = site.path().join("src.bin");
    fs::write(&src, vec![11u8; (DEFAULT_BLOCK + 7) as usize]).unwrap();
    let blob = store.put_file(&src).unwrap().blob;

    let objects = site.path().join("objects");
    let victim = fs::read_dir(&objects)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .flat_map(|e| fs::read_dir(e.path()).unwrap().flatten())
        .find(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .expect("库里该有块文件");
    let mut raw = fs::read(victim.path()).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 0xFF; // 静默损坏：单字节翻转
    fs::write(victim.path(), &raw).unwrap();

    let err = store.verify(&blob).expect_err("损坏必须被查出");
    assert!(
        err.to_string().contains("块校验失败"),
        "错误必须说清是损坏而不是缺失：{err}"
    );

    let dest = site.path().join("out.bin");
    let err2 = store.read_blob(&blob, &dest).expect_err("重建同样必须拒");
    assert!(err2.to_string().contains("块校验失败"), "{err2}");
    assert!(!dest.exists(), "校验失败时不许留下半文件");
}
