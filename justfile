# 显示可用 recipe.
[private]
default:
    @just --list

# 构建当前平台的 release 二进制, 产物位于 target/release.
dist:
    cargo build --release --locked

# 运行全部测试.
test:
    cargo test

# 运行 clippy 检查.
lint:
    cargo clippy --all-targets
