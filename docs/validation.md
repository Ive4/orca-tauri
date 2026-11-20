# 交付验证记录

2026-11-20 验证记录。前半部分记录 Linux x86_64 上的检查和 Windows 安装包静态审计。Apple Silicon 本机打包与运行见文末。

## 自动化验证

| 项目 | 实际状态 | 覆盖范围 |
| --- | --- | --- |
| `cargo test --locked --no-default-features` | **22 passed / 0 failed / 1 ignored fixture** | 配置、真实进程生命周期、日志脱敏、安全解包与活动版本事务。 |
| `cargo clippy --locked --no-default-features --all-targets -- -D warnings` | 最终版本通过 | 包括离线安装示例与测试代码。 |
| `cargo fmt --all --check` | 最终版本通过 | 全部 Rust 源码。 |
| `node scripts/test-pairing-init.mjs` | 18 项全部通过 | 未保存配对、已迁移设备凭据、清 profile、同端口换主机、损坏存储、来源边界等。 |
| Windows 进程模块 `cargo check --target x86_64-pc-windows-msvc --tests` | 通过 | 独立使用真实 windows 0.61.3 crate 检查 Job、CreateProcess、句柄、Windows 专用回归代码。 |
| PowerShell parser | 两个脚本通过 | 使用官方 PowerShell 7.7，未执行 Windows 内核运行步骤。 |
| 嵌入 C# P/Invoke helper | `Add-Type` 编译通过 | C# 语法与类型检查，不等于在 Windows 调用 kernel32 成功。 |
| GitHub Actions YAML / 内嵌 JavaScript | 解析、语法检查通过 | 没有在外部仓库执行、创建 PR 或发布 Release。 |
| 完整 Tauri 桌面层 `cargo check --locked --all-targets` | Linux 上通过 | 使用真实 GTK 3.24.49 / WebKitGTK 2.52.6 开发依赖，Tauri 2.12.7；未运行 GUI。 |
| 完整桌面层 `cargo clippy --locked --all-targets -- -D warnings` | Linux 上通过 | 包含桌面协调、托盘、窗口和各插件调用。 |

被忽略的 Rust 测试是用于真实进程树回归的子进程 fixture，其他测试会显式启动它；不是忽略一个失败断言。
Windows 特有测试会强杀持有 Job 的 owner，再验证 core 与孙进程退出。该测试已经写入并进行类型检查，**仍需 Windows 原生运行**。

针对并发生命周期 fixture 的一次不稳定结果，测试侧增加了排他随机目录、端口场景串行化和精确失败原因断言；随后连续 10 轮相关回归通过，最后整仓再次通过。生产进程逻辑未为掩盖测试而修改。

## 真实官方安装包

使用真实 `stablyai/orca v1.4.264` Windows installer，静态解包，没有执行安装器：

| 检查 | 实际结果 |
| --- | --- |
| 安装器大小 | 228,269,752 bytes |
| 实测 SHA-256 | `676c32d9eb955a314356a220ffd8afbefc214f0f348a324620628353ad4535f3`，匹配锁定值 |
| NSIS 内部文件 | 唯一 `$PLUGINSDIR/app-64.7z` |
| Orcad 模板 schema | 4 |
| 程序版本 / 提交 | 1.4.264 / `686bbb9c28edeccc652f2f345be92bdb7dfc3976` |
| 内容摘要 | 322 个 common + 14 个 Windows x64 原生文件 + 1 个 browser，共 337 个文件全部匹配 |
| 槽文件总字节 | 78,050,972 |
| 浏览器资产 | 1,375 个清单文件全部存在，符合提取白名单 |
| 浏览器资产总字节 | 79,078,084 |
| Windows 特殊分流 | packagedRipgrep / packagedBrowser 实际路径与提取器一致 |

这些结果证明打包脚本不是基于猜测目录编写；它们不证明纯 Node 在 Windows Job 内已经启动成功。
固定摘要和来源在 `core-version.json`；摘要核对汇总见 `installer-audit.json`。

## 未完成的运行资格

- 没有 Windows 11 桌面会话，因此没有实际运行 NSIS 安装、WebView2 配对/销毁/重开、全局快捷键或开机自启。
- 没有在 Windows 上执行真实 core、node-pty/ConPTY、候选更新、硬杀壳进程和 Job 回收。
- 没有测量 Rust 壳、orcad、WebView2 或 Agent 的后台私有工作集；没有声称达到 10 MB。
- 整仓 Windows 交叉构建曾因 MSVC C 工具链/SDK 不可用受阻，不能将进程模块的独立类型检查扩大为完整 Windows 壳构建通过。

`release-core.yml` 的 Windows gate 会实际执行模块图加载、ConPTY、readiness、HTML 和 Job 清理；失败不会生成可发布 ZIP。
GitHub Windows runner 的 gate 也不能替代 Windows 11 上的 WebView2 交互与内存人工验收。

## 发现并修复的集成问题

1. 官方 `orca serve` 可回退 Electron，改为固定 Node 入口。
2. 进程创建后再绑定 Job 有父进程崩溃空窗，改为创建时绑定。
3. 产品版本与 orcad 内容槽版本不同，健康校验使用 manifest 的 `ORCA_VERSION`。
4. Windows 终止异步，失败候选需等待整树退出后才能使用相同端口启动旧版。
5. 创建过窗口不代表配对已经保存；依据实际浏览器持久身份处理配对 fragment。
6. 忙碌的更新操作不能阻塞现有窗口唤醒、销毁，也不能无声丢弃重复启动请求。
7. 原子指针/配置替换成功后，目录 fsync 失败不能返回错误的“尚未提交”语义。
8. 下载期间退出不等待下载锁；内核停止使用其自己的生命周期锁和最终超时回收。

安全与功能验收的完整人工检查表见 `upstream-compatibility.md`。

## Apple Silicon macOS 打包验收

实测环境为 Apple Silicon arm64、macOS 27.2.1（26B131）、Rust stable 1.100.0。构建命令见 [README](../README.md#apple-silicon-macos-构建)。

| 检查 | 实际结果 |
| --- | --- |
| Core 提取 | 官方 Orca 1.4.265 的 `darwin-arm64` 模板通过 424 个 Core 文件摘要校验，Web 清单中的 1,520 个文件全部存在。锁定的 Node 24.22.1 通过原生模块加载和真实 PTY 冒烟。 |
| `.app` 与签名 | `target/aarch64-apple-darwin/release/bundle/macos/Orca Tauri.app` 为 101.96 MiB，bundle ID 为 `dev.orca-tauri.desktop`。`codesign --verify --deep --strict` 通过，使用 ad hoc 签名。 |
| 最低系统版本 | `Info.plist` 的 `LSMinimumSystemVersion` 为 `13.5`。外壳与 ZIP 内的 Node 均为 arm64 Mach-O，`otool -l` 报告的 `minos` 均为 `13.5`。 |
| 应用资源 | `bootstrap-core.zip` 的 SHA-256 与同包的 `bootstrap-core.json` 匹配。产品版本是 `v1.4.265`，运行时槽版本是 `0.1.0+8c6bf3905e8d`。 |
| 离线安装 | `install_core` 示例使用最终包中的 ZIP 在另一空白测试目录通过安装、真实 Core 就绪、活动指针和停止后端口释放检查。 |
| 首次自动安装 | 通过包内可执行文件启动。独立 `ORCA_TAURI_HOME` 只有测试配置，Core 和运行时数据目录起始为空。应用在随机空闲回环端口启动所拥有的内置 Node，校验就绪和 PTY 后写入 `core/active.json`。活动版本与包摘要匹配，`/web-index.html` 返回 HTTP 200，托盘启动没有主窗口。 |
| WKWebView 配对与重连 | 另一个隔离 profile 以 `open_window_on_start=true` 启动。Orca Computer Use 的无障碍树与截图确认实际 Orca Web 窗口。WKWebView 保存了该回环来源的运行时身份，Core 保存了迁移后的设备密钥记录。关闭原生窗口后 Core 继续提供 HTTP 200，再次启动同一包唤起新窗口并复用该配对身份。 |
| 正常退出 | 分别对默认托盘和带 WKWebView 的包实例发出 SIGTERM。应用以 0 退出，退出时所属 Core 进程组已清空，端口已释放，`active.json` 保留。 |
| 源码检查 | `cargo test --locked --no-default-features` 通过 22 项，按预期忽略 1 个子进程 fixture。`cargo fmt --all --check`、无桌面特性的 Clippy、带桌面特性的 `cargo check --locked --all-targets` 和 18 项配对脚本测试均通过。 |

带窗口退出的初版会在销毁最后一个窗口时响应 Tauri 的隐式退出事件，提前结束正常清理。修正后由清理流程显式结束应用，最终包在同一验收场景通过。

本次使用已安装官方 Orca 的测试机。Core 监听进程为包内 Node。WKWebView 用例中还观察到上游从官方应用启动的浏览器 sidecar，该 sidecar 属于测试 Core 的进程组并随之回收。干净机器、macOS 13.5 实机、强杀后的进程回收、启动耗时与内存对照、Apple notarization 尚未验收。
