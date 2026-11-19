# twosync 的一键验证（§12 承诺的"不依赖托管 CI 也能跑全套"）。
# 无托管 CI 环境 ⇒ 用 justfile 替代；装 just：cargo install just。

# 默认：双引擎全套测试 + clippy
check: test test-bplus clippy

# 默认引擎（lsm_tree）全套测试
test:
    cargo test

# 换 bplus_tree 跑同一套用例（§10.8 引擎矩阵）
test-bplus:
    TWOSYNC_TEST_ENGINE=bplus_tree cargo test

# clippy 零警告
clippy:
    cargo clippy --all-targets -- -D warnings

# release 构建（量二进制体积对 NFR-9 的 ≤15 MB）
release:
    cargo build --release

# 性能台架（一次一个引擎，进程峰值 RSS 只能这样归因）
perf:
    TWOSYNC_PERF_ENGINE=lsm_tree cargo test --release --test engine_perf -- --ignored --nocapture

# criterion 缩样档（跨引擎只可比三格，见 README）
bench-quick:
    TWOSYNC_BENCH_QUICK=1 cargo bench