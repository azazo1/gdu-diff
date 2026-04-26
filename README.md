# gdu-diff

`gdu-diff` 是一个基于 Rust 的 TUI 工具, 用来分析多个 `gdu-go` 导出快照, 对比目录空间占比的变化, 并以接近 `gdu-go` 的交互方式浏览差异。

它支持两类主要工作流:

- 保存某个目录的历史快照
- 将历史快照和当前扫描结果, 或一份 / 多份 JSON 快照, 放在一起对比

## 核心概念

### metric 是什么

界面中的 `metric` 指当前用于计算大小和占比的度量方式, 有两种:

- `disk`: 磁盘实际占用大小, 来自 `gdu` 导出中的 `dsize`
- `apparent`: 表观大小, 来自 `gdu` 导出中的 `asize`

默认使用 `disk`, 因为它更接近真正占掉了多少磁盘空间。使用 `-a` 或界面中的 `a` 可以切换到 `apparent`。

### Share 和 ShareD

- `Share`: 当前条目在当前父目录中的占比
- `ShareD`: 首个快照到最后一个快照之间, 占比变化了多少个百分点

### 当前目录变化

顶部 `Overview` 会显示当前视图所在目录自己的变化信息:

- `baseline -> latest`
- `delta`
- `P`: 在父目录中的占比变化
- `R`: 在根目录中的占比变化

根目录没有父目录, 所以只显示 `R`。

## 依赖

运行本项目需要:

- Rust
- `gdu-go` 或 `gdu`, 并且命令在 `PATH` 中

`shot` 和“当前目录 vs 历史快照”的模式都会直接调用外部 `gdu-go`/`gdu` 生成 JSON。

## 快速开始

### 编译

```bash
cargo build
```

### 运行测试

```bash
cargo test
```

### 保存快照

为某个目录生成并保存一份 `gdu` JSON 快照:

```bash
gdu-diff shot /path/to/dir
```

如果不传路径, 默认保存当前目录:

```bash
gdu-diff shot
```

### 使用历史快照对比当前目录

如果传入一个目录, 或不传参数, 程序会:

1. 在数据目录中找到这个目录最新的一份历史快照
2. 重新扫描当前目录
3. 打开 TUI 对比“历史快照 vs 当前结果”

```bash
gdu-diff /path/to/dir
gdu-diff
```

### 使用单个 JSON 对比目录

如果只传入一份 `.json` 文件, 程序会重新扫描当前工作目录, 然后对比“这份快照 vs 当前目录”:

```bash
gdu-diff old.json
```

也可以显式指定要扫描的目录, 语法是 `gdu-diff [目录] snapshot.json`:

```bash
gdu-diff /path/to/dir old.json
```

### 直接对比两份或多份 JSON

如果传入两个或以上 `.json` 文件, 程序会直接加载这些快照并打开 TUI:

```bash
gdu-diff old.json new.json
gdu-diff 2026-01.json 2026-02.json 2026-03.json
```

无论是单个 JSON 对比目录, 还是直接对比多份 JSON, 所有快照的根目录都必须一致。根目录不同会直接报错, 不进入 TUI。

### 切换 apparent size

```bash
gdu-diff -a /path/to/dir
```

### 仅显示目录

默认会显示目录和文件。如果只想看目录:

```bash
gdu-diff --dirs-only /path/to/dir
```

## 数据目录

历史快照保存在 `dirs-next::data_dir()` 对应的数据目录下, 再拼上 `gdu-diff/snapshots`。

例如在 macOS 上通常会类似:

```text
~/Library/Application Support/gdu-diff/snapshots/
```

每个被追踪的目录会映射到一个独立子目录。原始绝对路径不会直接作为文件名使用, 而是会被编码成安全的目录名, 避免非法字符和分隔符问题。过长的路径名会自动截断, 并追加稳定哈希, 避免单个目录名超过文件系统限制。

## 界面操作

- `j/k` 或方向键: 上下移动
- `,` / `.`: 上下翻页
- `l` / `Enter`: 进入目录
- `h` / `Backspace`: 返回上级
- `s`: 按最新大小排序
- `d`: 按大小变化排序
- `p`: 按占比变化排序
- `n`: 按名称排序
- `a`: 切换 `disk` / `apparent`
- `f`: 切换是否显示文件
- `c`: 复制当前选中项的相对路径, 没有选中项时复制当前视图目录
- `C`: 复制当前选中项的绝对路径, 没有选中项时复制当前视图目录
- `q`: 退出

TUI 默认按 `Delta` 排序。

## 界面说明

### Overview

顶部区域显示:

- 根目录
- 快照数量
- 快照范围
- 当前视图路径
- 当前 `metric`
- 当前排序方式
- 当前视图目录自身的变化摘要

### Children

中间表格显示当前目录下的子项:

- `Type`
- `Change`
- `Name`
- `Latest`
- `Delta`
- `Share`
- `ShareD`

如果目录为空, 或当前只显示目录而目录里只有文件, 会显示空提示。

`Change` 会标记条目的变化类型:

- `+`: 新增
- `-`: 删除
- `~`: 改变
- `=`: 未变

### Selected

底部区域显示当前选中项的详细信息, 包括:

- `Item`
- `Size`
- `Share`
- `Timeline`

名称和路径采用接近 fish 的配色风格:

- 目录为蓝色, 并带 `/`
- 隐藏文件偏灰
- `.json` 偏洋红
- 常见源码后缀偏绿

## 代码结构

### [src/main.rs](src/main.rs)

CLI 入口, 负责:

- 解析参数
- 判断当前是 `shot`, 目录对比, 还是 JSON 直接对比
- 创建 `Analysis` 和 TUI `App`

### [src/gdu.rs](src/gdu.rs)

`gdu` 导出层, 负责:

- 解析 `gdu-go` 的 JSON 树格式
- 调用外部 `gdu-go`/`gdu` 导出新的快照

### [src/store.rs](src/store.rs)

历史快照存储层, 负责:

- 解析数据目录
- 根据目录路径定位对应快照桶
- 保存 `shot`
- 找到某个目录最新的一份历史快照

### [src/analysis.rs](src/analysis.rs)

分析层, 负责:

- 将快照树拍平成统一路径索引
- 聚合多快照的大小和占比时间线
- 计算 `Latest`、`Delta`、`Share`、`ShareD`
- 为当前目录和子项生成展示数据

### [src/tui.rs](src/tui.rs)

交互界面层, 负责:

- 渲染 `Overview`、`Children`、`Selected`
- 处理键盘导航
- 维护当前路径、排序方式、显示模式

## 开发建议

如果你要继续开发, 一般从这几个入口开始:

- 改 CLI 行为: `src/main.rs`
- 改快照格式和扫描流程: `src/gdu.rs` / `src/store.rs`
- 改排序、统计口径、时间线: `src/analysis.rs`
- 改布局、配色、键位: `src/tui.rs`

比较常见的开发循环:

```bash
cargo fmt
cargo test
cargo run -- assets
```

如果要验证快照工作流:

```bash
cargo run -- shot assets
cargo run -- assets
```
