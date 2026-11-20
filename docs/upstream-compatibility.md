# 官方内核打包契约与验收边界

核对日期：2026-11-20。初始锁定版本：`stablyai/orca v1.4.264`，提交
`686bbb9c28edeccc652f2f345be92bdb7dfc3976`。

这份锁定记录已经核对官方源码、Release 资产元数据和实际下载安装包的 SHA-256。
也已静态解包真实 NSIS 产物，验证 337 个 Windows 槽文件的摘要和 1375 个浏览器客户端文件的存在性。
整个检查没有执行安装器或内核。它不是一次已经完成的 Windows 11 实机认证。
`core-version.json` 的初始 `validation.status` 明确标记为
`archive-verified-awaiting-windows-smoke`。只有实际运行 Windows 提取与冒烟流程后才生成验证报告和压缩包。

## 1. 从官方源码确认的内容

| 项目 | 已核实的契约 | 对本项目的影响 |
| --- | --- | --- |
| `orca serve` | CLI 会选择 orcad；选择失败时可以回退 Electron。打包的 macOS 还存在默认保留 Electron 更新交接的分支。 | 壳子直接执行独立 Node 加 `orcad-server.js`，不依赖这条自动选择路径。 |
| 模板 | 官方安装包的 `resources/orcad-template/` 包含通用 JS、平台原生模块和 `orcad-template.json`；模板本身不包含 Node。 | 读取模板清单并逐文件验证，额外下载精确匹配的 Node 运行时。 |
| Windows 原生模块 | 模板包含 `win32-x64` / `win32-arm64` 的原生槽；Windows x64 中包含 ConPTY、node-pty、进程树及安全模块。 | 现有发布配方产出 Windows x64。Apple Silicon macOS 优先补齐独立配方和实机验收。 |
| Node 位置 | 内核检查 `.runtime-node`、`.server-target`、Node 版本和可执行文件的真实位置。 | 布局必须保留 `orcad/` 与 `runtimes/node-<sha256>/node.exe` 的同级关系。 |
| Web UI | 官方打包器排除 `out/web`，将可配对的浏览器客户端放在 `app.asar.unpacked/out/renderer/`。 | 按 `.orca-web-client-files.json` 的清单提取到独立 `web/`。不能只复制 Electron renderer 的普通 `index.html`。 |
| HTTP 探测 | `/web-index.html` 是浏览器入口；`/` 也映射到它。此 HTTP handler 没有 `/health` API。 | `health_path` 仅作静态页面存活探测。进程身份与内核健康来自子进程 stdout 的 readiness。 |
| 健康信息 | `--json` 输出 `type=orca_server_ready`，包含 `health.pid`、构建标识、Node 标识和 `terminalDaemon.selfTest`。 | 需要核对直接启动的 PID、监听端口、Node/内核版本及终端自检，不能以端口打开作为全部成功条件。 |
| 配对 | `pairing.webClientUrl` 是 HTTP URL，凭证位于 `#pairing=...` fragment。 | 第一次打开使用内核提供的配对 URL，WebView 使用持久 profile 保存客户端身份；不把配对 URL 写进日志。 |
| 首次打开延迟 | headless serve 创建 offer 时不传 TTL；该 offer 不按桌面 UI 的配对时限过期。 | 托盘启动后延迟打开首次窗口，不需要仅为续签 offer 重启内核。用户手动撤销配对、清除 profile 等情况仍需重新配对。 |
| 终端生命周期 | 本地终端 daemon 最终使用 Node `child_process.fork`，设置 `detached` 和 `windowsHide`。独立的 `spawnOutsideJob` 入口用于特定 SSH breakaway 启动参数。 | 不能因为 bundle 包含 breakaway 代码就断言本地启动必须逃出 Job；发布前仍须在严格 Job 下实际启动、执行 PTY 自检并清理整棵树。 |

### 初始版本的固定来源

| 内容 | 固定值 |
| --- | --- |
| 官方安装包 | `orca-windows-setup.exe`，来自 `v1.4.264` Release |
| 安装包 SHA-256 | `676c32d9eb955a314356a220ffd8afbefc214f0f348a324620628353ad4535f3` |
| Node 版本 | `24.22.1`，使用上游该提交的 Node pin |
| Node 压缩包 | `node-v24.22.1-win-x64.zip` |
| Node 压缩包 SHA-256 | `d2046d28c2c3dcd211ac5145c0bf30e2047ef9dfb306693c8c6e2eccd8b95fa5` |
| Node 可执行文件 SHA-256 | `d702daa815eabbff763c7d3808e235cf031cf2ca70211b56847bc72f51e240e5` |

查询时官方最新稳定版已经是 `v1.4.265`。初始实现仍使用用户给出的 `v1.4.264` 作为具体兼容性基线，
没有把“版本更新”当作“已验证”。巡检工作流可以提出下一版的候选 PR。

## 2. 实际包布局

```text
core-manifest.json
upstream-lock.json
orcad/
  orcad-server.js
  orcad.js
  .version
  .runtime-node
  .server-target
  out/cli/index.js
  out/mobile-web/...
  node_modules/...
  ripgrep/...
  ...模板列出的 worker 与原生文件
runtimes/
  node-<executable-sha256>/node.exe
web/
  web-index.html
  .orca-web-client-files.json
  assets/...
licenses/
  ORCA-LICENSE
  NODE-LICENSE
  ...安装包中已有的许可证/声明文件
```

`app.asar` 用于识别和检查官方安装包布局，不进入运行包。整个 Electron 主进程和 Chromium 外壳不进入该包。
如果模板声明可选的 `agent-browser` 二进制，脚本会按其摘要复制，并纳入上游槽版本计算。
浏览器自动化实际使用的浏览器运行时及其功能仍需单独验收；这不是在此仓库重写 Electron 的浏览器组件。

启动方式由包内 manifest 表达，示意如下：

```text
<core>/runtimes/node-<sha>/node.exe
  <core>/orcad/orcad-server.js
  --bind 127.0.0.1
  --port <configured-port>
  --web-client-root <core>/web
  --json
```

工作目录为 `<core>/orcad`；`ORCA_USER_DATA` 指向独立的可写用户数据目录，
`ORCA_VERSION` 使用上游算法计算的 `0.1.0+<slot-content-hash>`。
Release 的产品版本 `v1.4.264` 和这个槽构建标识承担不同职责，不应混用。

## 3. 提取与验证

在 Windows x64 上准备 PowerShell 7.2+ 和 7-Zip，然后在仓库根目录运行：

```powershell
pwsh -NoProfile -File ./scripts/extract-headless.ps1
```

已有官方安装包时可复用下载；仍然强制匹配锁定的摘要：

```powershell
pwsh -NoProfile -File ./scripts/extract-headless.ps1 `
  -InstallerPath C:\Downloads\orca-windows-setup.exe `
  -OutputDirectory ./dist-core
```

脚本只解包安装器，不运行安装器。它检查安装包版本、模板版本、清单摘要、Node 摘要与运行路径，
然后运行独立验证。任一检查失败则不生成可发布 ZIP。

成功产物：

- `orca-core-v0.0.1-1.4.264-x86_64-pc-windows-msvc.zip`
- 同名 `.zip.sha256`，一行标准 SHA-256 文件校验记录。
- `orca-core-v0.0.1-1.4.264-x86_64-pc-windows-msvc-validation.json`，只含不带凭证的验证结果。

验证过程把子进程在 **创建时** 放入非 breakaway 的 Windows Job：使用
`PROC_THREAD_ATTRIBUTE_JOB_LIST`、`CREATE_SUSPENDED`、`CREATE_NO_WINDOW`，配置
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 后才恢复线程。
这项验证同时避免“先运行，再绑定 Job”造成的抢跑窗口。

验证报告区分以下项目：

1. 上游 `--orcad-smoke-load-check`，确保纯 Node 模块图可加载。
2. 直接使用包内 node-pty 创建一个真实 `cmd.exe` 终端并观察它正常结束。
3. 内核 readiness 中 PID、固定端口、平台、Node 版本、槽版本及终端自检。
4. 配对 URL 的来源和形状，以及 `/web-index.html` 返回 HTML。
5. 终止 Job 后 `ActiveProcesses` 回到零。

其中第 2 项是必要的：上游健康报告包含 `coverage`，有的平台可能只提供握手覆盖，
不能把握手成功表述为已经跑过真实 PTY 创建。

已经追踪的 Windows 本地启动链是 `startOrcadDaemon` → `initDaemonPtyProvider` →
`createOutOfProcessLauncher` → `spawnDaemonChildProcess` → `forkProcess` → Node `child_process.fork`。
它没有调用 `spawnOutsideJob`。Windows 的 daemon executable relocation 也只对 Electron 的
`app.asar` 根路径启用，纯 Node 模式跳过。特殊 SSH breakaway 模式则由独立的命令行 flag 触发；
本项目不传该 flag，也不在 Job 上允许 breakaway。

如果 daemon 启动失败，上游的 unsupervised 模式可以退回 in-process PTY；supervised 模式会报错。
本项目不能把这种退化当作完整成功，因此验证仍检查 readiness 中的终端自检。

脚本不启动 Tauri/WebView2，所以报告中的 `webview2_interaction` 明确为
`not-tested-by-this-gate`。GitHub 的 `windows-latest` 也是服务端 runner，不等同于 Windows 11 桌面实机。

## 4. 工作流与发布边界

`check-upstream.yml` 每天检查稳定版，将安装包摘要、源码提交和匹配的 Node pin 放进候选 PR。
它不会自动合并。升级分支如果已有人工修改，会保留该分支，不强推覆盖。

因为用 `GITHUB_TOKEN` 创建 PR 通常不会再触发 `pull_request` 工作流，巡检会额外显式触发
`release-core.yml` 的 `workflow_dispatch`，且 `publish=false`。
仓库需允许 Actions 创建 PR；此工作流还需要 `actions: write` 来显式发起验证任务。

`release-core.yml` 在 PR/手工验证时只产出构建 artifact。合并到 `main` 后，或在 `main` 上手工选择
`publish=true` 后，通过验证的产物才进入 `core-windows-v0.0.1-1.4.264` 这样的 Release。
先建立 draft，所有产物上传完成后才公开；已发布的同版本产物不会被覆盖。
发布时 `make_latest=false`，避免把内核版本误标为桌面外壳的最新安装包。
若仓库默认分支不是 `main`，需同步调整发布工作流的分支条件。

如果上游改了 NSIS 嵌套压缩包名称、模板 schema、Node pin 格式或 renderer 清单，流程会明确失败。
这时应审阅新结构并修改提取器，不应通过删除校验或启用 Electron 回退来绕过失败。

## 5. Windows 11 上仍需完成的验收

| 场景 | 验收标准 |
| --- | --- |
| 无窗口冷启动 | 没有 Tauri 主窗口/WebView2；内核 ready，托盘可操作。 |
| 首次配对 | 从托盘打开官方浏览器 UI，完成身份保存并能访问工作区。 |
| 销毁与重开 | 连续关闭/重开 20 次，关闭采用销毁；已配对身份保留，能重新连接原内核，窗口关闭不终止任务。 |
| 快捷键/重复启动 | 快捷键正常呼出/销毁；第二次启动唤醒同一实例，无第二个内核。 |
| 实际代理执行 | 本地 Codex、终端和至少一个子 Agent 工作流运行、结束和重连正常。 |
| 壳子硬终止 | 任务管理器结束壳子后，属于其 Job 的内核、PTY 与子 Agent 退出；不误杀其他 Orca/Codex 实例。 |
| 正常退出 | 当前 Windows 路径执行 `TerminateJobObject` 并等待 Job 中进程清空；尚未接入经验证的上游优雅停止协议。Unix 路径发送 SIGTERM，宽限后再强制清理。 |
| 更新成功 | 准备新的不可变版本目录，销毁旧窗口并停止旧内核；候选内核启动且健康验证通过后，才提交 active 指针，再允许打开 UI。 |
| 更新失败 | 损坏下载、摘要不符、缺文件、健康失败和端口占用均不能留下错误的 active 指针。旧内核能恢复。 |
| 掉电/终止中断更新 | 分别在下载、解包、停止、切换、健康验证时硬终止；下次启动能恢复一致的可启动版本。 |
| 配对恢复 | 清除 WebView profile、撤销当前设备、浏览器存储异常时有可执行的重新配对路径。 |
| 功能差异 | 分别检查插件、浏览器自动化、文件选择、系统打开、通知、登录回调等与 Electron/原生 API 有关的功能。 |
| 内存 | 同时记录壳子、orcad、daemon 和 WebView2 子进程的私有工作集/提交量。不能把“Rust 托盘约 10 MB”当作已测总占用。 |

浏览器客户端可用不代表 Electron 桌面功能一项不差。这个外壳的兼容边界是上游提供给浏览器客户端的能力。
严格收回整棵子进程树也意味着退出外壳或更新内核时，本次内核启动的活动终端/Agent 会被停止，
即使上游原本允许某些 daemon 跨内核重启存活。
内核二进制回滚也不等同于用户数据迁移回滚；候选内核如果修改了共享 profile 的数据结构，
需要依据上游版本兼容性处理，外壳不会声称已经撤销那些数据修改。

### WebView 重新创建时的配对保护

`assets/pairing-init.js` 在上游脚本执行前检查当前页面自己的存储。它只处理
`http://127.0.0.1:<port>/web-index.html` 的顶层页面，验证 fragment 中官方 v2 runtime offer 的
监听地址、公钥和 profile `installationId`。只有已经存在同一服务端的有效持久环境时，才清除
URL 中的旧 pairing fragment，让官方页面走 `use-stored-environment` 分支。

这个检查不修改任何存储、不读取其他页面、不调用 Rust IPC、不输出凭证。它也不将“窗口已经创建”
误当成“前端已经保存凭证”：第一次立即关闭、资源加载失败、清除 profile 或禁用存储后再次打开，
仍保留启动 offer。`node scripts/test-pairing-init.mjs` 使用合成数据检查这些情形。

原因是上游在同一服务端的 runtime offer 再次出现时，仍会提升 pairing revision 并重新保存 offer 的 bearer token。
这样可能替换已经迁移为 device key 的本地凭据。仅匹配端口也不够，另一个服务端可能占用相同端口；
因此保护逻辑同时匹配服务端公钥和上游 descriptor 定义的安装身份。`machineBinding` 仅作为迁移提示，
按上游规则不单独作为服务端身份。

## 6. 官方一手资料

以下链接固定在初始兼容性基线，避免之后 `main` 变更影响复核：

- [v1.4.264 Release](https://github.com/stablyai/orca/releases/tag/v1.4.264)
- [独立 orcad 启动与 Electron 回退选择](https://github.com/stablyai/orca/blob/v1.4.264/src/cli/runtime/serve/startup/serve-orcad-launch.ts)
- [实际 orcad 命令参数](https://github.com/stablyai/orca/blob/v1.4.264/src/main/orcad/serve-startup/orcad-command-arguments.ts)
- [官方模板打包](https://github.com/stablyai/orca/blob/v1.4.264/config/scripts/orcad-distribution/template/packaged-orcad-template.cjs)
- [官方打包目录/asarUnpack](https://github.com/stablyai/orca/blob/v1.4.264/config/electron-builder.config.cjs)
- [Node 精确版本与二进制摘要](https://github.com/stablyai/orca/blob/v1.4.264/src/shared/node-runtime/node-runtime-pin.ts)
- [运行时位置与版本检查](https://github.com/stablyai/orca/blob/v1.4.264/src/main/orcad/runtime-launch/orcad-bundled-runtime.ts)
- [本地 daemon 的 fork 参数](https://github.com/stablyai/orca/blob/v1.4.264/src/main/daemon/launch/daemon-launched-child-spawn.ts)
- [Node fork 包装器](https://github.com/stablyai/orca/blob/v1.4.264/src/packages/process-host/src/fork-process.ts)
- [daemon 失败与退化分支](https://github.com/stablyai/orca/blob/v1.4.264/src/main/orcad/daemon/orcad-daemon-supervision.ts)
- [仅用于特定入口的 Windows breakaway launcher](https://github.com/stablyai/orca/blob/v1.4.264/src/main/orcad/runtime-launch/windows-breakaway-launcher.ts)
- [官方 artifact 文件清单](https://github.com/stablyai/orca/blob/v1.4.264/src/shared/remote-runtime/orcad-distribution/orcad-artifacts.ts)
- [readiness 格式](https://github.com/stablyai/orca/blob/v1.4.264/src/main/server/serve-readiness.ts)
- [终端健康与覆盖范围](https://github.com/stablyai/orca/blob/v1.4.264/src/main/orcad/diagnostics/orcad-health.ts)
- [静态 Web UI 路由](https://github.com/stablyai/orca/blob/v1.4.264/src/main/runtime/rpc/transports/websocket/static-web-client-handler.ts)
- [Web 配对 URL](https://github.com/stablyai/orca/blob/v1.4.264/src/main/runtime/rpc/server/runtime-rpc-pairing-types.ts)
- [headless 配对有效期规则](https://github.com/stablyai/orca/blob/v1.4.264/src/main/runtime/rpc/server/pairing/pairing-offer-expiry.ts)
- [浏览器的配对启动分支](https://github.com/stablyai/orca/blob/v1.4.264/src/renderer/src/web/pairing/web-pairing.ts)
- [浏览器存储及 pairing revision](https://github.com/stablyai/orca/blob/v1.4.264/src/renderer/src/web/pairing/web-runtime-environment.ts)
- [服务端公钥与安装身份比较](https://github.com/stablyai/orca/blob/v1.4.264/src/shared/remote-runtime/host-status/runtime-host-pairing-identity.ts)
- [官方 LICENSE](https://github.com/stablyai/orca/blob/v1.4.264/LICENSE)
- [GitHub：用 GITHUB_TOKEN 触发工作流的例外](https://docs.github.com/en/actions/how-tos/writing-workflows/choosing-when-your-workflow-runs/triggering-a-workflow#triggering-a-workflow-from-a-workflow)

上游仓库的 LICENSE 为 MIT，并要求分发副本保留版权和许可声明。提取器保留该声明、
Node 发布包的 LICENSE、模板内 ripgrep 声明以及安装包根目录和资源目录已有的许可证文件。
若之后向包内加入新的独立依赖，应同时加入该依赖实际要求的声明文件。
