# orca-tauri

Tauri v2 桌面壳在系统托盘常驻，按需创建官方 Orca 的浏览器客户端窗口，关闭时销毁 WebView。打包应用随附无头 Core，启动时验证并激活。

**先开发 Apple Silicon macOS 版，再完成 Windows 11 x64 版。** GitHub Actions 为两个平台打包随附 Core 的 Orca-Tauri，也支持独立发布 Core。

1. **macOS**。从官方安装包提取 `darwin-arm64` Core 槽及同版 Web 资源，用锁定的 arm64 Bun 启动。实机验证进程回收、配对与重连，再用同版本官方 Orca 对比启动到可操作、会话切换和整套进程内存。
2. **Windows**。使用已有的 Windows x64 提取、ConPTY 和 Job 回收实现，完成 WebView2 实机配对、安装和性能验证。

本项目没有前端框架或 npm 构建步骤。`assets/index.html` 只是 Tauri 构建所需的空资产；实际窗口加载本机内核的官方 `/web-index.html`。`assets/pairing-init.js` 是一个很小、带回归测试的配对兼容脚本。

## 实际实现

| 目标 | 实现 |
| --- | --- |
| 按需创建 WebView | `tauri.conf.json` 中 `windows: []`。普通启动等 Core 就绪后打开主窗口，后台启动只创建托盘并启动 Core。打包应用启动时自动安装随附的初始内核或更新版本。 |
| 关闭即释放视图 | 原生关闭使用默认销毁；快捷键与托盘关闭使用 `WebviewWindow::destroy()`，不调用 `hide()`。 |
| 单实例 | 官方 single-instance 插件首先注册；重复启动唤醒现有窗口，或在 core 就绪后创建窗口。 |
| 快捷键 | 默认 `Ctrl+Shift+O`；macOS 为 `Command+Shift+O`。窗口存在则销毁，否则创建。 |
| 开机启动 | 官方 autostart 插件，托盘开关；默认关闭，自启时传 `--background`。 |
| 独立内核 | 版本目录在用户可写路径，core 不嵌入壳程序。 |
| 无 Electron 回退 | 直接用包内运行时启动 `orcad-server.js`。macOS 使用 Bun，Windows 使用 Node。 |
| Windows 树回收 | 创建时用 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 加入 Job；`KILL_ON_JOB_CLOSE`，无 breakaway，Job 句柄不可继承。 |
| 隐藏控制台 | 创建 core 使用 `CREATE_NO_WINDOW`；release 壳使用 Windows GUI 子系统。 |
| 身份与健康验证 | 读取自己子进程的 JSON readiness，核对 PID、端口、运行时槽版本和终端自检，再检查真实浏览器 HTML。 |
| 内核更新 | 打包应用通过应用更新获得随附内核。开发版可从配置仓库选择当前平台的稳定 Core Release。候选经 SHA-256 校验、安全解包和启动验证后才会激活。 |
| 更新恢复 | 下载/解包/候选验证失败时保留活动指针，等待候选进程完全退出后恢复原内核。 |
| 日志与代理 | core 标准输出中的配对秘密不落盘；stderr 做保守脱敏。localhost 检查直连；子进程与下载代理分别配置。 |

## 1. 从托盘查看版本

托盘的 **设置 → 关于与更新** 显示外壳和当前激活的内核版本。

1. **内核版本**显示 `<打包修订>-<官方版本>`。macOS 当前锁定 `0.0.4-1.4.266`，Windows 为 `0.0.1-1.4.266`。本地 Web UI 随内核更新。旧包缺少修订字段时，显示 `0.0.0-<官方版本>`。
2. **外壳版本**来自 `src-tauri/Cargo.toml`，当前为 `0.1.0`。Tauri 安装包和外壳更新使用这个版本。
3. **查询 Orca 官方最新稳定版…**读取 `stablyai/orca` 的最新稳定 Release，和当前激活的内核的官方基线比较。同一主、次版本可看到补丁版本号差值。

安装或升级应用后，启动时会自动验证并启用随附的新内核。「关于与更新」只显示一个已激活的内核版本。已安装的同版或更新内核继续使用。随附候选验证失败时，等候选进程退出后恢复原内核。

两个平台的 `core-version*.json` 分别固定 `version`（官方 Release 标签）和 `package_revision`（本项目打包修订）。同一个官方版本再次打包发布时，递增该平台的 `package_revision`。升级官方基线后可从 `0.0.1` 开始。清单中的 `environment.ORCA_VERSION` 供启动时核对运行时槽身份。

Windows v1.4.266 官方安装器已核对摘要，静态解包后逐项验证了 433 个 Core 槽文件及 1,513 个 Web UI 文件。锁文件标记为 `archive-verified-awaiting-windows-smoke`，Windows 原生冒烟仍待执行。版本升级时需要核对 readiness、配对存储和打包契约。

完整一手资料、固定摘要和实际包布局见 [docs/upstream-compatibility.md](docs/upstream-compatibility.md)。

### Core 源码维护

[Ive4/orca-core](https://github.com/Ive4/orca-core) 是官方 `stablyai/orca` 的 fork，用于独立维护 Orcad 源码。维护分支以本项目锁定的官方提交为起点。

| 平台 | 维护分支 | 初始基线 |
| --- | --- | --- |
| Apple Silicon macOS | [maint/macos-arm64](https://github.com/Ive4/orca-core/tree/maint/macos-arm64) | `v1.4.265` |
| Windows x64 | [maint/windows-x64](https://github.com/Ive4/orca-core/tree/maint/windows-x64) | `v1.4.264` |

当前打包仍按本仓库的锁文件取用官方源码或安装包，macOS 构建时应用 `scripts/macos-data-plugins.patch`。后续接入 fork 产物时，应固定源码提交和产物摘要，并在更新源发布前完成目标平台的核心验证。

## Apple Silicon macOS 构建

构建需要 Rust stable、Xcode Command Line Tools、Python 3.9+、Node.js 24+ 与 npm，以及与 `core-version.macos-arm64.json` 匹配的官方 `/Applications/Orca.app`。Node.js 用于构建上游源码，应用随包运行 Bun。

```sh
./scripts/build-macos.sh
```

默认生成 `target/aarch64-apple-darwin/release/bundle/macos/Orca-Tauri.app`，随附 Bun 1.4.4 和从固定源码编译的 SQLite 3.54.1。独立内核 ZIP 和 SHA-256 位于 `dist-core/macos-arm64-bun-plugins/`。可用 `--bun-archive` 和 `--sqlite-archive` 复用已有下载。

可用 `--orca-app /path/to/Orca.app` 指定官方应用，或用 `--download-orca` 下载。脚本校验官方签名和文件摘要，再以锁定的上游源码、pnpm 依赖锁和项目补丁重建 `orcad-server.js` 与 Web 界面。可用 `--upstream-source-archive` 指向已下载的固定提交源码压缩包。打包前会运行原生模块加载、真实 PTY 和回环 HTTP 检查。应用声明的系统下限为 macOS 13.5。本地构建默认使用临时签名。

点击菜单栏图标查看任务，在弹窗内选中任务或点击「打开 Orca」打开主窗口。**Command+Shift+O** 也可直接打开主窗口。使用独立的绝对路径 `ORCA_TAURI_HOME` 可以在空数据目录中试用随包 Core。

**首次打开与钥匙串。** WebKit 用应用的钥匙串项目保护本地配对密钥。若确认是刚安装的可信 Orca-Tauri 请求访问「Orca-Tauri WebCrypto Master Key」，选「始终允许」可为当前签名保存授权。临时签名（ad hoc）每次重建的身份可能不同。要让后续构建沿用授权，在已安装 Developer ID Application 证书和对应私钥的 Mac 上用同一个 `APPLE_SIGNING_IDENTITY` 运行 `scripts/build-macos.sh`。从已有临时签名迁移到正式签名时，系统可能要求确认一次。

**macOS 窗口自动化。** 用 `./scripts/build-macos.sh --smoke-test` 构建 `Orca-Tauri Smoke.app`。每次构建使用独立的 Bundle ID，登录钥匙串处于解锁状态时，WebKit 会为测试包创建自己的主密钥。用新的 `ORCA_TAURI_HOME` 启动每个新测试包。测试重开窗口或重启应用时，复用同一个包和数据目录。

```sh
./scripts/build-macos.sh --smoke-test
smoke_home="$(mktemp -d)"
printf '%s\n' '{"port":26768,"notifications":false}' > "$smoke_home/config.json"
ORCA_TAURI_HOME="$smoke_home" \
  "target/aarch64-apple-darwin/release/bundle/macos/Orca-Tauri Smoke.app/Contents/MacOS/orca-tauri"
```

在 Orca 窗口的 **设置 → 插件** 中浏览官方目录，也可从本地目录或固定 Git 提交安装。该宿主支持数据插件、受信任的 Worker、脚本面板及允许在浏览器运行的自定义编辑器。宿主能力和验收范围见 [macOS 插件兼容](docs/macos-data-plugins.md)。旧版 Node 打包应用的首启和窗口生命周期记录见 [macOS 打包验收](docs/validation.md#apple-silicon-macos-打包验收)。

## 2. Windows 开发与运行

准备 Rust stable、Visual Studio C++ Build Tools（含 Windows SDK）以及 WebView2 Runtime。打包精简内核另外需要 **PowerShell 7.2+ 和 7-Zip**。壳不要求用户另外安装 Node；精简 core 包自带经过摘要校验的官方 Node。

在仓库根目录：

```powershell
rustup update stable
cargo install tauri-cli --version "^2" --locked
cargo test --locked --no-default-features
cargo tauri dev
```

首次启动后，系统托盘会显示图标。可打开弹窗的「设置 → 关于与更新」查看 Core 安装状态。

### 方式 A：先用本地安装包完成一轮验证

```powershell
pwsh -NoProfile -File ./scripts/extract-headless.ps1
```

脚本下载固定摘要的官方安装器，**只解包，不执行安装器**。按官方模板提取 Windows 原生槽、匹配 Node 与浏览器资产，运行真实 node-pty/ConPTY、readiness、HTTP 与 Job 回收测试。任一步失败均不生成可发布 ZIP。

然后退出托盘里的壳，以避免本地服务端口被占用，安装刚产生的包：

```powershell
$coreArchive = Resolve-Path ./dist-core/orca-core-v0.0.1-1.4.266-x86_64-pc-windows-msvc.zip
$coreDigest = (Get-FileHash -LiteralPath $coreArchive -Algorithm SHA256).Hash.ToLowerInvariant()
cargo run --locked --no-default-features --example install_core -- `
  "$coreArchive" v1.4.266 "$coreDigest"
cargo tauri dev
```

离线安装示例也会启动 core 并检查真实 readiness，健康通过才提交活动指针，随后停止测试 core。它不会接管已有 Orca 实例。上面现场计算的摘要适用于刚由自己构建、已核对上游来源的 ZIP；下载第三方 ZIP 时，应使用发布者提供并核实过的摘要。

### 方式 B：从你自己的 GitHub Releases 安装

将本项目提交到自己的 GitHub 仓库。在 `main` 上运行 **Qualify and release headless core**，选择 `publish=true`。验证成功后生成如下资产：

```text
Release: core-windows-v0.0.1-1.4.266
orca-core-v0.0.1-1.4.266-x86_64-pc-windows-msvc.zip
orca-core-v0.0.1-1.4.266-x86_64-pc-windows-msvc.zip.sha256
orca-core-v0.0.1-1.4.266-x86_64-pc-windows-msvc-validation.json
```

从托盘“设置 → 高级 → 打开配置文件”打开 `config.json`，填写 `update_repository`，例如 `your-name/orca-tauri`。**编辑配置后退出并重新启动壳，配置才生效**；只点“重启内核”不会重新读取配置。

随后使用“设置 → 关于与更新 → 检查内核更新”。当前下载实现面向公开 Releases；没有内置 GitHub PAT 管理。GitHub Actions 的 **Build and release Orca-Tauri** 会在编译时自动将 `github.repository` 写为默认核心更新仓库，本地构建可通过配置指定仓库。

### 生成 Windows 安装器

```powershell
$env:ORCA_TAURI_REPOSITORY = 'your-name/orca-tauri'
cargo tauri build --bundles nsis
```

产物位于 `target/release/bundle/nsis/`。只需要可执行文件时，也可使用 `cargo build --release --locked`，产物为 `target/release/orca-tauri.exe`。正式分发的签名与信誉需要使用你自己的发布身份配置；仓库不包含签名密钥。

### GitHub Actions 打包、发布与更新

- **仅打包**。在 Actions 手动运行 **Build and release Orca-Tauri**。下载 `Orca-Tauri-<外壳版本>-macos-arm64.zip` 或 `Orca-Tauri-<外壳版本>-windows-x64-setup.exe`。两者都随附已通过平台检查的内核，以及记录平台、外壳、内核和 UI 版本的 JSON。
- **正式发布**。首次用 `cargo tauri signer generate -w /path/to/orca-tauri.key` 生成一对更新签名密钥。将公钥文件内容写入仓库变量 `TAURI_UPDATER_PUBLIC_KEY`，将私钥文件内容写入仓库 Secret `TAURI_SIGNING_PRIVATE_KEY`，有密码时再设置 Secret `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。后续版本沿用同一对密钥。
- **macOS 应用签名**。在仓库 Secrets 中配置 `APPLE_CERTIFICATE`（Base64 编码的 Developer ID Application `.p12`）和 `APPLE_CERTIFICATE_PASSWORD`。导出一套签名身份并沿用原应用标识。连续版本使用同一个签名身份，macOS 才能保留安装后授予的钥匙串权限。
- 修改 `src-tauri/Cargo.toml` 的应用版本，运行 `cargo check --all-targets` 同步 `Cargo.lock`，提交两者后推送对应的标签（例如 `v0.1.1`）。两个平台均打包并签名成功后，工作流发布应用 Release、`latest.json` 和 SHA-256 校验清单。`latest.json` 按 `darwin-aarch64` 和 `windows-x86_64` 提供签名、更新地址、安装包地址和随附的内核／UI 版本。macOS 标签发布要求配置 Apple 签名证书，手动打包允许使用临时签名。Windows 安装器未做 Authenticode 签名。
- **更新应用**。从公开 Release 安装的版本可在托盘选“设置 → 关于与更新 → 检查应用更新”。下载并校验签名成功后才会关闭窗口、回收内核任务并安装新版本。首次分发和手动打包产物可从 Release 下载安装器。
- **更新内核和 UI**。打包应用随应用更新内核和 Web UI。用 **Qualify and release headless core** 验证两个平台的锁定包。在 `main` 手动运行并启用 `publish` 可发布独立内核，例如 `core-macos-v0.0.4-1.4.266` 或 `core-windows-v0.0.1-1.4.266`。未随附内核的开发版可通过托盘「设置 → 关于与更新 → 检查内核更新」使用配置的 GitHub Releases 安装独立内核，也可从「高级」回退至上一版。

## 3. 数据布局与配置

默认根目录为 Windows `%USERPROFILE%\.orca-tauri`，Unix 为 `~/.orca-tauri`。设置绝对路径环境变量 `ORCA_TAURI_HOME` 可以迁移整个根目录；`core_directory` 可以单独调整核心安装位置。

```text
~/.orca-tauri/
  config.json
  core/
    active.json
    versions/<version>-<target>-<sha>/
    staging/
  orca-data/             # 独立 Orca 用户数据，不覆盖官方安装的数据
  webview/               # 浏览器身份/缓存，销毁窗口时保留
  logs/
```

持久浏览器 profile 保留的是磁盘数据，不意味着保留隐藏 WebView。Windows/Linux 使用明确的 profile 目录；macOS WKWebView 使用平台持久存储机制，不能将其路径控制等同于 WebView2。

### 从官方 Orca 导入

在 Apple Silicon macOS 上，官方 Orca 和 Tauri 内核均为 v1.4.266 时，选托盘「设置 → 从官方 Orca 导入…」，确认后即可在「打开 Orca」查看导入的项目、工作树和已保存的对话及附件。官方 Orca 可以保持运行。导入会停止 Tauri 的后台任务并重启独立内核。

每次导入使用官方版当前资料的项目和本机官方内核的内置聊天记录。导入前的 Tauri 数据完整保存在根目录下的 `orca-data-before-import-*`。要恢复，退出 Orca-Tauri，将当前 `orca-data` 移到其他位置，再把对应的备份改名为 `orca-data`。项目文件仍在原路径，SSH 连接及托管账号需要在 Tauri 中重新配置。

### 在 Orca-Tauri 中查看官方 Orca 会话

1. 在官方 Orca 的「设置 → Orca 远程服务器 → 共享此电脑」中选择「此电脑」，生成访问链接并复制 `orca://pair?…` 链接。
2. 从 Orca-Tauri 托盘「设置」打开「连接官方 Orca…」，粘贴访问链接并连接。以后用同一个托盘入口重新打开。
3. 用托盘「打开 Orca」切回 Orca-Tauri 自己的工作区和会话。

官方会话由运行中的官方 Orca 提供。两个入口分别保存配对，两个内核分别管理自己的数据。更改 Orca-Tauri 的端口会改变 WebView 的存储来源，官方入口需要重新配对。

在 Orca 中选择「简体中文」即可使用 Bots 补译。壳在 WebView 加载官方语言目录时按键补齐缺失词条，官方目录一旦提供对应译文就会自动优先使用。补译键已与 Orca v1.4.266 的英文目录核对。[bots-i18n.zh.json](assets/bots-i18n.zh.json)

完整示例为 [config.example.json](config.example.json)，主要配置如下：

| 字段 | 默认值 / 作用 |
| --- | --- |
| `port` | macOS 默认 `16768`，其他平台默认 `6768`。仅监听回环，支持 1024–65535。macOS 的旧 `6768` 配置遇到占用时改试 `16768`。 |
| `core_directory` | `null`；使用 `<根目录>/core`。自定义时必须为绝对路径。 |
| `update_repository` | 本地构建为 `null`；填实际的 `owner/repo`。CI 可编译写入默认值。 |
| `download_proxy` | `null`；显式值支持 HTTP/HTTPS 代理，未指定则保留 reqwest 环境代理逻辑。 |
| `start_core_on_launch` | `true`；有已安装 core 时在托盘启动它。 |
| `open_window_on_start` | `true`；普通启动时直接打开主窗口。`--background` 始终只在托盘启动。 |
| `global_shortcut` | `CommandOrControl+Shift+O`。快捷键冲突时保留托盘操作并提示。 |
| `autostart` | `false`；通常通过托盘开关修改。 |
| `notifications` | `true`；原生操作失败或更新结果提示。 |
| `environment` | `{}`；注入 core 及其子 Agent 的自定义环境变量。 |
| `startup_timeout_secs` | `60`，允许 5–300。 |
| `shutdown_timeout_secs` | `8`，允许 1–30。 |

例如本地代理端口为 7890：

```json
{
  "download_proxy": "http://127.0.0.1:7890",
  "environment": {
    "HTTP_PROXY": "http://127.0.0.1:7890",
    "HTTPS_PROXY": "http://127.0.0.1:7890",
    "NO_PROXY": "localhost,127.0.0.1,::1"
  }
}
```

将这些字段合并到现有配置即可。壳的健康检查强制 `no_proxy()` 且禁止重定向；子进程 `NO_PROXY` 自动补上本地回环地址，并保留已有内网排除项。壳不会把 `environment` 中的代理设置隐式当作自己的下载代理。`ORCA_USER_DATA`、`ORCA_VERSION` 由包契约管理，配置覆盖会报错。

## 4. 进程回收和关闭语义

### Windows 11

进程以 `CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT` 创建。在创建调用中通过 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 加入专属 Job，然后恢复线程。它避免了“先启动再 Assign”的抢跑，也避免了“先 suspended，再 Assign”之间壳崩溃所留下的挂起孤儿。

Job 配置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，无 breakaway，且不允许子进程继承 Job 句柄。退出、崩溃、任务管理器强杀壳时，操作系统关闭最后一个 Job 句柄，从而回收属于该 Job 的 core 与后代。创建失败时不回退到不受管理的启动。

正常更新或重启会等待 Job 中的 `ActiveProcesses` 归零，再启动下一个内核。强制终止是异步操作；仅请求 TerminateJobObject 后立即启动旧 core 会产生端口抢占竞态，本实现对这个情况有回归测试。

**默认 Windows 停止是终止 Job 并等待树退出。** 当前没有把未核实的 HTTP/RPC 地址伪装成 Orca 的优雅关机 API。代码保留显式 `ShutdownRequest` 扩展点，但默认没有启用。退出壳、停止或更新内核都会结束本次内核启动的终端和 Agent；关闭 WebView 窗口则不会停止这些任务。

Job 管理的是其拥有的进程树。通过外部服务、计划任务、WMI 或独立 broker 发起的任务，不能归纳为此 Job 内的子进程保证；本实现不会扫描并误杀机器上其他 Codex/Orca 进程。

### Linux / macOS

使用独立进程组，先 SIGTERM，等待宽限期后 SIGKILL。Linux 增加 `PR_SET_PDEATHSIG` 保护直接子进程。普通进程组无法保证所有 `setsid()` 后代、守护化后代在父壳 SIGKILL 时全部消失；macOS 也没有等价的通用 Job Object。

macOS 首版验收包括正常退出、启动失败回收和同端口重启。外壳被强制终止后的整树回收需要单独验证监督机制。Linux 的 WebKit 网络进程可能由 WebKit 上下文保留，测内存时应计入仍归属于该应用的进程。

## 5. 更新事务

正常路径：检查当前平台的稳定核心 Release → 下载并验证摘要 → 在 staging 安全解包 → 放入唯一版本目录 → 停旧 core → 启候选 core → 验证 stdout 身份、PTY 自检与浏览器 HTML → 原子提交 `active.json` → 恢复原来打开的窗口。

独立更新下载和解包期间，旧 core 持续运行，现有窗口可以唤醒或销毁。候选验证成功前，活动指针保持为原版本。打包应用在下次启动时会重新尝试随附的候选。进程切换期间打开窗口的请求会排队。

安全解包拒绝相对路径穿越、绝对路径、Windows ADS/设备名、符号链接、大小写冲突等。限制下载 1 GiB、单文件 512 MiB、总展开 2 GiB、80,000 条目以及 64 KiB manifest。下载资产名、目标架构和版本必须相符。

**版本回退不会撤销用户数据迁移。** `orca-data` 独立于不可变程序版本，新核心如果迁移了数据库，恢复旧二进制不等于恢复数据。跨数据格式升级前应在 core 停止后备份数据；本实现没有静默复制或覆盖用户工作区。

SHA-256 来自同一 Releases 仓库，证明下载内容与发布摘要一致，不是独立签名。发布权限属于信任边界。未自动删除旧版本目录，便于回退与检查；清理历史版本时必须保留 `active.json` 指向的 current/previous，且先停止 core。

## 6. 目录与 CI

```text
src-tauri/src/
  config.rs              配置、回环URL、清单到LaunchSpec转换
  desktop.rs             插件、操作协调、更新/回退、RunEvent
  lib.rs / main.rs       核心导出与桌面入口
  process/windows.rs    原子Job绑定、句柄继承白名单
  process/unix.rs        进程组与Linux父退出信号
  process/mod.rs         CoreManager、readiness、健康与回收
  window.rs              按需创建、销毁、导航边界
  tray.rs                原生托盘与动态状态
  updater.rs             下载、验证、安全解包与原子提交
assets/pairing-init.js   配对身份兼容（无Rust IPC）
scripts/
  extract-headless.ps1   固定来源提取器
  verify-core.ps1        真正Windows/Node/PTY/Job验证
  test-pairing-init.mjs  配对脚本行为测试
.github/workflows/
  ci.yml                 核心测试、桌面编译、脚本语法
  build-shell.yml        手工构建Windows NSIS与EXE
  check-upstream.yml     定时提出候选升级PR
  release-core.yml       Windows验证后发布独立core资产
```

巡检只提出升级 PR，不自动合并。使用 `GITHUB_TOKEN` 创建的 PR 通常不触发其他 `pull_request` 工作流，因此巡检额外发起 `workflow_dispatch(publish=false)`。配置 Actions 创建 PR 权限后，此链条才能工作。发布条件以 `main` 为基准；如果使用其他默认分支，需要修改相应条件。

版本提升不代表浏览器配对等私有契约仍兼容。请审阅验证报告，再在 Windows 11 上做人工验收。源码中的浏览器配对兼容逻辑固定依据 v1.4.264，当前核心升级资格测试不替代 WebView2 交互检查。

## 7. 验证

不安装 GTK/WebView 开发库，也能运行进程、配置和更新核心测试：

```text
cargo test --locked --no-default-features
cargo clippy --locked --no-default-features --all-targets -- -D warnings
node scripts/test-pairing-init.mjs
node scripts/test-window-init.mjs
cargo fmt --all --check
```

Windows 专用测试会创建持有 Job 的 owner、core 和孙进程，强杀 owner 后检查后代终止。Linux 测试实际启动子孙进程，验证正常停止、健康失败、失败候选立即同端口重启、日志脱敏和端口冲突；被标记 ignored 的测试只是被其他测试启动的子进程 fixture。

交付时的实际运行记录见 [docs/validation.md](docs/validation.md)。Windows 11 人工验收表见兼容性文档。内存验收应分别统计壳、Node/orcad、PTY/Agent、WebView2 的私有工作集和提交量，并比较冷启动、窗口打开、窗口关闭后这三个状态。**约 10 MB 是目标，不是本项目已经测出的结果。**

## 8. 常见问题

**启动后没有窗口**：检查 `open_window_on_start` 是否为 `true`，并查看托盘中的 Core 状态。登录自启后可从托盘“打开 Orca”或使用快捷键。

**更新菜单不可用**：先填实际 `update_repository`，退出并重开壳。首次本地构建没有预设 GitHub 仓库。

**回环端口被占用**：在配置中换用空闲端口并重启壳。macOS 上同时使用官方 Orca 时，可固定使用 `16768`，使配对存储来源保持一致。

**打开后要求重新配对**：检查是否清理了 WebView profile、撤销了设备或改变了数据目录。已保存身份会复用；若内核无法提供配对 offer，先看日志。“内核管理 → 重启内核 / 重新配对”会停止本内核拥有的活动任务并产生新的启动配对信息。

**Web UI 可用但某项 Electron 功能缺失**：支持范围由上游浏览器客户端决定。文件打开、浏览器自动化、认证回调、桌面插件和通知等需要分别验收；壳不会伪造 Electron preload API。

**日志里看不到完整 readiness**：这是有意的。配对地址含访问凭证，只保存在运行进程内存及官方浏览器自身存储中。不要把真实配对 fragment 放进 issue 或构建 artifact。

## 许可证

新编写的壳代码使用 MIT。精简核心保留官方 Orca、Node、原生模块与工具自身的许可证和 notices；参见包中的 `licenses/` 和兼容性文档。项目是独立外壳，不是官方 Orca 发行版。

### 托盘任务弹窗

点击系统托盘图标查看任务。再次点击图标、点击外部或按 Esc 会关闭弹窗并释放它的 WebView。点击任务或「打开 Orca」才打开主窗口。

- 探头猫在 macOS 使用系统模板图标，随菜单栏外观切换，深色菜单栏显示白色线稿。Windows 使用适合浅色任务栏的深色线稿。
- **进行中**显示等待输入、受阻和正在运行的任务。需要处理的任务排在最前。「最近结束」显示过去 24 小时内，当前核心仍报告的已结束任务。
- **设置 → 关于与更新**显示外壳和内核版本。确认安装应用更新后，应用下载并验证安装包。下次启动时自动验证并启用随附的新内核和 Web UI。
- **设置 → 高级**提供重启、停止，以及配置和日志目录。停止或重启会结束当前内核的任务。

弹窗显示本应用内核报告的任务。通过「连接官方 Orca」配对的服务器在主窗口里查看。
