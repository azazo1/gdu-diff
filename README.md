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

默认无参数启动时, 顶部 `range` 会显示实际的 shot 文件名, 例如 `shot-1712345678 -> current`.

根目录没有父目录, 所以只显示 `R`。

## 依赖

运行本项目需要:

- Rust
- `gdu-go` 或 `gdu`, 并且命令在 `PATH` 中

持久化 `shot` 会直接调用外部 `gdu-go`/`gdu` 先生成 JSON, 然后在进程内用 zstd 压成 `.json.zst` 快照。用于当前对比流程的临时扫描结果仍然保持为未压缩 JSON, 避免额外的压缩和解压开销。

程序内部现在基于 `tokio` 异步运行时驱动. 快照读取, 外部 `gdu-go`/`gdu` 扫描进度采集, 以及 TUI 的键盘事件循环都已经改成异步流程, 不再依赖手写多线程轮询框架.

## 快速开始

### 编译

```bash
cargo build
```

发布用的 release 构建可以直接使用 `just`:

```shell
just dist
```

### 运行测试

```bash
cargo test
```

### 保存快照

为某个目录生成并保存一份 `gdu` 快照:

```bash
gdu-diff shot /path/to/dir
```

如果不传路径, 默认保存当前目录:

```bash
gdu-diff shot
```

每个目录默认最多保留 3 份较新的历史快照。保存新的 `shot` 后, 更旧的快照会自动删除, 但会额外长期保留一份磁盘占用最低的历史快照, 方便和历史低水位做对比。

也可以在需要传入快照文件的位置使用 `-<n>` 这种别名, 表示这个目录按"从新到旧"排序后的第 `n` 份历史 `shot`:

```bash
gdu-diff -1
gdu-diff /path/to/dir -2
gdu-diff -1 old.json
gdu-diff old.json -2
```

其中:

- `-1` 表示最新的一份历史 `shot`
- `-2` 表示第二新的历史 `shot`
- `-3` 表示第三新的历史 `shot`

如果 `-<n>` 单独出现, 或者和其他快照文件一起直接对比, 默认按当前工作目录查找对应历史 `shot`。如果写成 `gdu-diff [目录] -<n>`, 则按这个目录查找。

### 使用历史快照对比当前目录

如果传入一个目录, 或不传参数, 程序会:

1. 在数据目录中找到这个目录最新的一份历史快照
2. 重新扫描当前目录
3. 在进入主 TUI 前显示加载界面, 展示当前阶段和状态
4. 打开 TUI 对比“历史快照 vs 当前结果”

```bash
gdu-diff /path/to/dir
gdu-diff
```

### 使用单个快照文件对比目录

如果只传入一份 `.json` 或 `.json.zst` 文件, 程序会重新扫描当前工作目录, 然后对比"这份快照 vs 当前目录":

```bash
gdu-diff old.json
gdu-diff -1
```

也可以显式指定要扫描的目录, 语法是 `gdu-diff [目录] snapshot.json`:

```bash
gdu-diff /path/to/dir old.json
gdu-diff /path/to/dir -2
```

### 直接对比两份或多份快照

如果传入两个或以上 `.json` 或 `.json.zst` 文件, 程序会直接加载这些快照并打开 TUI:

```bash
gdu-diff old.json new.json
gdu-diff -1 old.json
gdu-diff old.json -2
gdu-diff 2026-01.json 2026-02.json 2026-03.json
```

无论是单个快照对比目录, 还是直接对比多份快照, 所有快照的根目录都必须一致。根目录不同会直接报错, 不进入 TUI。

### 启动加载界面

在真正进入主 TUI 之前, `gdu-diff` 会先显示一个加载界面, 用来展示:

- 当前动作, 例如读取快照, 扫描当前目录, 构建分析索引
- 主进度条, 现在按步骤权重累计, 轻量步骤占比会更低
- 每个步骤自己的子进度条
- 当前阶段的细节状态

其中, 读取快照时, 程序会按文件大小和一组偏保守的固定常数估算整个"读取 + 解压 + JSON 解析"阶段的大致耗时, 用这个估算值展示一个假进度条, 避免文件已经读完但 `serde_json` 仍在解析时看起来像是卡住。当前对比流程里刚扫描出来的临时 `current.json` 不会再压缩, 持久化的 `.json.zst` 则会在后台流式解压后再解析。

默认情况下, 最后几步通常是:

- `Build analysis`: 把快照树展开并建立对比索引, 这样后续 TUI 可以按路径快速查询和排序
- `Prepare interface`: 根据当前根目录生成首屏表格和初始选中状态, 这一步通常很轻

像 `Resolve target directory` 和 `Prepare interface` 这类轻量步骤, 现在在主进度条里的权重会低于扫描目录, 读取快照, 构建分析索引这些重步骤, 这样主进度条末尾不会因为轻量收尾动作显得走得过慢。

如果底层调用的是 `gdu-go` / `gdu` 扫描当前目录, 加载界面会尽量展示其扫描状态。同时会给扫描步骤补一个非线性的假进度, 先增长, 在 `95%` 停住, 等外部扫描真正完成后再补到 `100%`。实际能否看到更细粒度的实时状态, 仍然取决于外部 `gdu-go` / `gdu` 在当前终端环境中的输出行为。

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

默认保存的新 `shot` 文件后缀是 `.json.zst`。程序仍然兼容直接读取旧的 `.json` 快照, 当前对比流程中的临时快照则继续使用 `.json`。

## 版本与发布

`--version` 显示的版本号在构建时自动生成, 基础版本号跟随最近一个版本 tag:

- 构建 commit 恰好是版本 tag 时, 直接显示该 tag, 例如 `v1.2.3`
- 构建处于非 tag commit 时, 在最近 tag 后追加 `-` 和 6 位短 commit hash, 例如 `v1.2.3-a1b2c3`
- 工作区还有未提交改动时, 分隔符改为 `^`, 例如 `v1.2.3^a1b2c3`

发布流程:

1. 在 `docs/changelog/` 下按版本号维护人工发布说明, 例如 `docs/changelog/0.1.0.md`
2. 提交版本号和说明文件, 然后直接用说明文件创建 annotated tag, 例如:

```shell
git tag -a "v0.1.0" --cleanup=verbatim -F "docs/changelog/0.1.0.md"
```

3. push tag 后, GitHub Actions 会构建 Linux, Windows, macOS 的 x86_64 和 aarch64 产物, 校验 tag 与包版本一致, 检查 tag annotation 与说明文件完全一致, 再自动创建或更新 GitHub Release
4. 也可以在 Actions 页面手动填写已有 tag 触发同一发布流程; tag 留空时只构建并上传 artifact, 不创建 release

构建, 校验或产物完整性检查失败时不会创建公开 release; 重跑同一 tag 会更新已有 release 的正文并覆盖产物。

## 界面操作

- `j/k` 或方向键: 上下移动
- `,` / `.`: 上下翻页
- `g` / `G`: 跳到第一条 / 最后一条
- `l` / `Enter`: 进入目录
- `h` / `Backspace`: 返回上级
- `s`: 按最新大小排序
- `d`: 按大小变化排序
- `p`: 按占比变化排序
- `n`: 按名称排序
- `a`: 切换 `disk` / `apparent`
- `f`: 切换是否显示文件
- `Space`: 切换当前项是否加入选区, 然后自动跳到下一项
- `c`: 复制当前选中项的相对路径, 没有选中项时复制当前视图目录
- `C`: 复制当前选中项的绝对路径, 没有选中项时复制当前视图目录
- `b`: 在当前视图目录打开 shell
- `r`: 在 current 对比模式下, 重扫当前视图目录
- `?`: 打开帮助
- `Esc`: 清空当前目录选区, 如果帮助已打开则关闭帮助
- `q`: 退出

TUI 默认按 `Delta` 排序。

`r` 只在"历史快照 vs 当前扫描结果"这类包含 `current` 的模式下可用。它会只更新当前目录对应的最新扫描子树, 不会重新读取 baseline 快照, 也不会整棵树全量重扫。

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

- `M`
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

`Space` 可以对当前行做离散标记. 标记成功后, 焦点会自动跳到下一项, 便于连续勾选. 排序变化不会清空这些标记, 但离开当前目录, 切换显示模式, 切换 `metric`, 或按 `Esc` 时会清空当前目录选区.

### Selected

底部区域会同时显示:

- `Marked`: 当前目录选区的汇总
- `Item`
- `Size`
- `Share`
- `Timeline`

`Marked` 会汇总当前选区的:

- 条目数量
- `+ / - / ~ / =` 各自数量
- `Baseline` / `Latest` / `Delta`
- 在当前父目录和根目录中的占比变化

如果终端宽高不足, 底部这块详情区域会自动隐藏, 把空间优先让给列表。

名称和路径采用接近 fish 的配色风格:

- 目录为蓝色, 并带 `/`
- 隐藏文件偏灰
- `.json` / `.json.zst` 偏洋红
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
- 使用进程内 zstd 流式压缩和解压快照

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
cargo clippy --all-targets --all-features
cargo run -- assets
```

如果要验证快照工作流:

```bash
cargo run -- shot assets
cargo run -- assets
```
