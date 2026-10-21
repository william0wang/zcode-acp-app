# ZCode ACP

简体中文 | [English](README.md)

[zcode-acp](https://github.com/william0wang/zcode-acp) 的应用端：在本机的原生
桌面工作台、Android 手机或任意浏览器中，操控这台机器上运行的 agent 会话。
所有端都通过 WebSocket 接入 zcode-acp Hub；传输协议契约见服务端的
[REMOTE-CLIENTS.md](https://github.com/william0wang/zcode-acp/blob/main/docs/REMOTE-CLIENTS.md)。

有两处标识刻意保留历史的 `zcode-acp-remote` 拼写，不要顺手改：桌面端的线上身份
（`zcode-acp-remote-desktop`，由 hub 匹配）和桌面端配置目录
（`~/.config/zcode-acp-remote`）。

## 三种形态

| 形态 | 技术栈 | 运行环境 |
| --- | --- | --- |
| **桌面工作台**（`src-gpui/`） | 原生 Rust，gpui-kit | macOS（未签名 `.app`） |
| **Android**（`src/` + `src-tauri/`） | Tauri 2 + React + TypeScript | Android APK |
| **Web**（同一份 `src/`） | 纯 SPA，不使用 Tauri API | 任意浏览器 |

桌面工作台是停靠式窗口：左侧图标栏切换面板，会话面板和配置面板停靠在带标签页的
聊天区旁边，右侧是当前会话的文件浏览器。Android 和 Web 共用同一套 React 代码；
桌面端是独立的产品线，不是它的移植（ADR-0009）。两端编译同一套 i18n 词表。

## 功能

各端共有：

- 接入现有会话，或在本机任意已知项目中新建会话（需 bridge 0.17.0+），
  也可浏览本机目录选择目标（需 bridge 0.65.0+）
- 聊天流式输出，支持 Markdown、代码高亮和 diff 视图
- 在发起请求的会话中审批工具权限、回答 AskUserQuestion 表单——离线期间发出的
  请求会在重连后补发
- 按会话的提示词队列，草稿在应用重启后保留
- 会话文件：会话目录树的面包屑浏览，文本按行窗口加载、图片预览、其他类型下载
- 机器级配置页：模型、用量统计、套餐配额与重置卡、系统状态、Skills、MCP 服务、
  Hooks、子代理、配置备份、动态工作流、应用更新
- 套餐配额窗口与重置卡，以及账户用量统计
- 断线自动重连并以 replay 追平消息；瞬时断连不弹错误提示
- 界面支持 English / 简体中文

仅桌面端：

- 每个会话一个标签页，各自持有连接；⌘1–9 切换标签页，⌘K 搜索会话，Esc 停止当前回合
- Hub 自举——没有 hub 在监听时，应用拉起已安装的 `zcode-acp-server` 守护进程并接入
  （ADR-0010）
- 桌面在场——工作台连接期间，新会话以无头形式孵化，不再弹出终端窗口（ADR-0010）
- hub 在本机且已安装 Zed 时，可在 Zed 中打开当前浏览的目录或文件（未安装则该入口不显示）

## 安装

**Android** —— 从 [Releases](../../releases) 下载最新 APK 侧载安装，
在应用内输入 Hub 地址和令牌。

**桌面端** —— 自行打包（不提供预构建产物），见[桌面工作台](#桌面工作台)。

**Web** —— 自行部署 SPA，见[独立 Web 部署](#独立-web-部署)。

## 服务端准备

每种形态都只是客户端，需要一个运行中的 zcode-acp-server hub：

1. 在你想操控其 agent 会话的机器上安装并运行
   [zcode-acp-server](https://github.com/william0wang/zcode-acp)。
2. 若 hub 会被其他机器访问，启动 bridge 时设置远程令牌
   （`ZCODE_ACP_REMOTE_TOKEN`）。
3. 通过 `https://` 暴露 hub（隧道即可——应用会自动把 https 升级为 wss），
   或在局域网内直接使用 `http://`。

在运行桌面工作台的机器上，hub 可以是本地的：没有 hub 在监听时桌面端会自己拉起。
令牌仍然必需——配置文件和 `ZCODE_ACP_REMOTE_TOKEN` 都没有时，应用会显示引导卡片，
把生成的令牌写入配置。

## 开发

```bash
pnpm install
pnpm dev                # 浏览器开发 http://localhost:5173（最快迭代）
pnpm tauri android dev  # 通过 adb 连接真机/模拟器
pnpm build              # 类型检查 + 打包到 dist/
pnpm test               # vitest
```

桌面工作台：

```bash
cargo check  --manifest-path src-gpui/Cargo.toml
cargo test   --manifest-path src-gpui/Cargo.toml -p app
```

## Android APK

```bash
pnpm build:android          # release APK -> dist/，需要签名密钥
pnpm build:android:debug    # debug APK -> dist/，无需密钥
```

Release 构建使用 `.signing/` 目录下的密钥签名（已 gitignore；请自行创建
`keystore.jks` 和 `keystore.properties`）。应用标识：`app.zcode.acp`
（安装后不可更改）。

图标集由 `scripts/gen-icon.mjs` 生成（SVG 经 `sharp` 转为 1024px 源图）——
修改设计后重新执行
`node scripts/gen-icon.mjs && pnpm tauri icon src-tauri/icons/app-icon.png`。

## 桌面工作台

```bash
./scripts/build-macos-gpui.sh   # 原生 gpui 构建 -> dist/ZCode-acp.app
pnpm build:macos                # WebView 外壳构建 -> dist/ZCode-acp.app
```

两种 macOS 构建写入同一个产物路径，按设计互相覆盖：原生工作台（gpui，桌面产品线）
和 Tauri WebView 外壳（手机端运行的那套 React 界面）。两者都未签名
（无 Developer ID / 公证）：在构建机上直接可用；拷到其他机器需绕过
Gatekeeper（右键 → 打开）。

## 独立 Web 部署

React 客户端是纯 SPA——未使用任何 Tauri API——因此 Web 构建可在 iOS、
桌面及任意浏览器中运行。自行部署即可，无需自建后端：hub 已设置
`Access-Control-Allow-Origin: *`。

**一键部署（Netlify）** —— 会把仓库 fork 到你自己的 GitHub 账户并自动构建：

[![Deploy to Netlify](https://www.netlify.com/img/deploy/button.svg)](https://app.netlify.com/start/deploy?repository=https://github.com/william0wang/zcode-acp-app)

**Cloudflare Pages** —— 两种方式：

- Git 集成：fork 仓库后，在 Pages 控制台连接该仓库，构建命令
  `pnpm build`，输出目录 `dist`，环境变量 `NODE_VERSION=22`。
- 本机直传：`pnpm deploy:web` 构建 `dist/` 并直接推送到**你自己**
  Cloudflare 账号的 Pages 项目，无需 Git 集成。复制
  `.env.local.example` 为 `.env.local`（已 gitignore）填入 API 令牌和
  项目名；不配令牌则走 wrangler 自己的登录流程
  （`pnpm dlx wrangler login`）。

**任意静态服务器** —— 仓库自带 `netlify.toml`；Vercel 会自动识别 Vite 构建；
也可以本地构建后上传：

```bash
pnpm build              # -> dist/（静态、自包含）
pnpm exec vite preview  # 本地预览构建产物
```

**由 hub 直接托管（同源）** —— 服务端开启静态托管（`ZCODE_ACP_WEB_DIR`，
或配置文件 `remote.webDir`）后，hub 自己服务这套构建：页面、REST API 和
WebSocket 同源——没有 CORS、没有混合内容限制，无需单独的静态主机；走隧道
时一个 https 地址就是全部。

```bash
pnpm build
ZCODE_ACP_WEB_DIR="$PWD/dist" zcode-acp hub   # 然后打开 http://localhost:8377
```

连接页的 hub 地址留空即可——默认取当前页面的地址，只需填令牌。

注意事项：

- 分离部署时，`https://` 页面只能连接 `wss://` —— 请输入经隧道暴露的 hub
  `https://` 地址。
- 令牌保存在浏览器 localStorage 中：不要在公用电脑上使用；一次性访问建议用
  无痕/隐私窗口。

## 发版

`pnpm release <version>` 一步完成版本号更新、Android APK 构建、提交、打 tag、
推送和发布 GitHub Release。桌面工作台单独构建分发（没有签名产物通道）。
细节见 `docs/RELEASING.md`。

## 文档

- `README.md` — English docs
- `CONTEXT.md` — 术语表
- `docs/adr/` — 架构决策记录

## 许可证

[MIT](LICENSE)
