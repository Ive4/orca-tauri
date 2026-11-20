# macOS 插件兼容

Apple Silicon macOS 包以 Orca v1.4.265、提交 `3534932c2d0ee3305df97c9689ea23efa207abdd` 为基线。窗口连接到随包提供的 Core 后，**设置 → 插件** 使用 Orca 的插件目录、搜索、详情和已安装列表。用户可从官方目录、本地目录或固定 Git 提交安装插件。目录安装校验发布提交和内容摘要，运行权限由用户审阅的指纹决定。安装使用不可变目录。主题由 Orca 的外观设置保存并绘制，禁用或卸载已选主题时回退到内置配色。

插件宿主支持应用配色、终端配色、文件图标主题、语法包、界面语言包和新建文件模板。获准后，Worker 在独立进程中执行命令，脚本面板和允许浏览器运行的 HTML 自定义编辑器使用有会话授权的沙箱。组件面板、需要隔离访客的编辑器以及需要桌面资源协议的字体或面板模块仍受宿主限制。VM 配方、插件状态栏项目、OS 钥匙串及桌面独有的文件保存或打开外部链接等能力也需要单独适配。官方目录在详情页展示具体的兼容性限制。

## 构建与更新

需要 Apple Silicon、Xcode Command Line Tools、Rust stable、Python 3.9+、Node.js 24+、npm 和匹配锁文件的官方 Orca.app。可为验收指定新的 `CARGO_TARGET_DIR`，把产物与正在使用的应用隔开。

```sh
./scripts/build-macos.sh
./scripts/build-macos.sh --runtime node
```

macOS 提取脚本校验签名、官方模板、源码压缩包和运行时摘要，以固定 pnpm 版本安装上游锁定依赖。它应用 `scripts/macos-data-plugins.patch`，复用上游的构建入口重建服务端和 Web 界面，继续随包使用官方原生模块和辅助文件。可以分别用 `--upstream-source-archive`、`--bun-archive`、`--sqlite-archive` 或 `--node-archive` 指定本地下载缓存。脚本会校验其内容。

随包的独立核心禁用上游自动启动已安装 Electron 浏览器辅助进程的行为。独立核心的浏览器自动化可使用显式配置的 `ORCA_BROWSER_EXECUTABLE`。

Core 归档保留发布版本 `v1.4.265`，Web 文件摘要进入服务端产物，再参与运行时槽版本计算。空配置目录首启会验证并启用随包归档。已有活跃槽的配置目录优先启动原 Core。若要在已有配置中启用此构建，先退出对应壳，再用已有的 `install_core` 示例安装新归档。用 `dist-core/macos-arm64-bun-plugins/bootstrap-core.json` 中的版本和 SHA-256 核对归档，Node 构建对应 `dist-core/macos-arm64-node-plugins/`。安装示例在候选启动检查成功后提交指针。

```sh
cargo run --locked --no-default-features --example install_core -- \
  dist-core/macos-arm64-bun-plugins/bootstrap-core.zip v1.4.265 <SHA256>
```

## 验收步骤

1. 用独立的绝对路径设置 `ORCA_TAURI_HOME`，并在该目录的 `config.json` 指定空闲回环端口。启动应用后确认 Core 就绪，托盘操作前应无 WebView。
2. 打开 Orca，进入 **设置 → 插件**，搜索 **Nord for Orca**。打开详情并点击 **安装**，确认主题启用并自动应用。
3. 关闭 Orca 窗口，再从托盘打开。在新窗口确认 Nord 仍生效。回到插件页，禁用并卸载主题，确认内置配色恢复并且安装列表为空。
4. 搜索并打开 **Excalidraw**，确认详情页显示当前宿主的限制。

样本的官方插件 ID 为 `stablyai.orca-nord-theme`，发布提交为 `07590d2c7bdc8c6c9299123106b9e0525656a411`，最低 Orca 版本为 `1.4.245`。目录中的 Excalidraw 用于检查不可用贡献的呈现。

## Apple Silicon 官方页面验收

当前 Worker 版在 Bun 和 Node Core 上使用要求 Orca 1.4.265 的本地测试插件完成验收。WKWebView 调用确认安装后仍待授权，批准后 TypeScript Worker 可调用宿主读写插件存储。脚本面板与 HTML 编辑器可获取沙箱文档并调用宿主，面板可接收状态变更推送。禁用后旧会话与命令均被拒绝，随后可卸载插件。Bun 应用包通过签名校验，包内 Core 与实际验收使用的归档一致。

以下记录来自上一轮的官方数据插件验收。

在 macOS 27.2.1 arm64 上，Bun 和 Node Core 均通过真实 PTY 检查。在独立 WKWebView 中点开插件页，搜索并从详情安装 Nord，确认主题立即生效。关闭该 WebView 后以同一测试配置重开，主题仍生效。禁用后恢复默认配色，卸载后清除安装记录。Excalidraw 详情显示宿主限制。测试使用临时本地配对，最终复验使用上游的令牌兼容路径来避开临时验收程序的钥匙串提示。

默认 Bun 应用包位于 `target/aarch64-apple-darwin/release/bundle/macos/Orca-Tauri.app`。该包通过签名检查，能在空配置下启用随包 Core，退出后归还测试端口。隔离配置中的旧 Core 也已成功切换到新版，并保留回退槽。

## Apple Silicon 简化页面的验收记录

验收使用 macOS 27.2.1 arm64、独立 `ORCA_TAURI_HOME` 和随机回环端口。两种正式包均通过 arm64 构建和 `codesign --verify --deep --strict`。原生自动化使用独立包标识的 Tauri 验收副本，Core 归档与对应正式包逐字节相同。验收代码在自己的 WKWebView 上操作网页控件，调用与正式包相同的原生建窗和销毁函数。验收代码仅位于隔离副本中。

| 项目 | Bun 1.4.4 | Node 24.22.1 |
| --- | --- | --- |
| 运行时槽 | `0.1.0+d44f60654909` | `0.1.0+16ef23c01102` |
| 冷启动无 WebView，主进程持有监听端口 | 通过 | 通过 |
| 配对后浏览目录，Excalidraw 的审阅按钮不可用 | 通过 | 通过 |
| 安装 Nord 2.1.0，先显示待启用 | 通过 | 通过 |
| 启用并应用后，正文背景绘制为 `rgb(46, 52, 64)` | 通过 | 通过 |
| 销毁窗口并新建后，主题仍启用并绘制同一颜色 | 通过 | 通过 |
| 禁用、卸载后，背景恢复为 `rgb(10, 10, 10)`，安装列表为空 | 通过 | 通过 |
| SIGTERM 正常退出，归还端口并回收自有 Core 进程组 | 通过 | 通过 |
| Git 传输范围 | 直连目录读取成功，直连安装遇到连接重置。在独立 HTTPS 镜像中用来自官方仓库的精确目录与发布提交完成全流程 | 整个流程直连官方仓库，安装时重试两次 |

Bun 验收镜像使用从 GitHub 获取且通过 `git fsck` 的目录提交 `1329848789e35eb24ccf1dc946cc360da0dac465` 和发布提交 `07590d2c7bdc8c6c9299123106b9e0525656a411`。Git 重写和证书信任仅存在于隔离验收进程的环境中。正式包继续使用官方 Git 地址。

Orca Computer Use 在独立包中观察到 Orca 设置和官方目录。屏幕锁定后，启用、重开和恢复以 WKWebView 的实际计算样式和原生窗口生命周期断言验证。此轮逐项交互验收覆盖 Nord 应用主题。终端主题、文件图标、语法、界面语言和新建文件模板的逐项交互验收留待后续。

该轮验收通过的附加检查包括 Core 原生模块加载、真实 PTY、回环 HTTP、上游 Node/Web 类型检查、RPC 参数目录校验、Rust 单测 22 项、Clippy、格式检查、全目标编译检查，以及配对与翻译脚本测试。本地日志在 `target/official-data-plugins/validation/`。Rust 测试清单中的子进程 fixture 由生命周期用例调用。
