# zcode-acp-remote

简体中文 | [English](README.md)

[zcode-acp-server](https://github.com/william0wang/zcode-acp) 的移动端远程客户端：
在 Android 手机上通过 WebSocket 接入 Hub，操控与编辑器相同的 agent 会话。
传输协议契约见服务端的
[REMOTE-CLIENTS.md](https://github.com/william0wang/zcode-acp/blob/main/docs/REMOTE-CLIENTS.md)。

既可接入现有会话，也可在本机任意已知项目中远程新建会话（需 bridge
0.17.0+）—— hub 会为该项目拉起一个无头 serve bridge，会话在其中运行。

## 功能

- 会话列表带实时活动徽标（运行中 / 空闲 / 刚结束），长按可远程关闭会话
- 远程新建会话：从本机已知项目列表中选择一个，CLI 会话即在其中启动（需 bridge 0.17.0+）
- 聊天流式输出，支持 Markdown、代码高亮和 diff 视图
- 审批工具权限请求、回答 AskUserQuestion 表单，仅在发起请求的会话中弹出；手机离线期间发出的提问会在重连后补发
- 按会话的提示词队列 —— 草稿在应用重启后保留
- 斜杠命令补全，含 `$` 前缀的 skills
- 用量配额卡片，绿→黄→红热力色显示
- 断线自动重连并以 replay 追平消息；瞬时断连不弹错误提示
- 界面支持 English / 简体中文

## 安装

从 [Releases](../../releases) 下载最新 APK 侧载安装，在应用内输入 Hub 地址和远程令牌即可。

本应用同时是一个纯 SPA —— 未使用任何 Tauri API —— 因此 Web 构建可在 iOS、
桌面及任意浏览器中运行，见 [独立 Web 部署](#独立-web-部署)。

## 服务端准备

本应用只是客户端，需要一个运行中的 zcode-acp-server hub：

1. 在编辑器中安装并运行
   [zcode-acp-server](https://github.com/william0wang/zcode-acp)
   （它把 agent 通过 Hub WebSocket 桥接出来）。
2. 启动 bridge 时设置远程令牌（`ZCODE_ACP_REMOTE_TOKEN`）。
3. 通过 `https://` 暴露 hub（隧道即可 —— 应用会自动把 https 升级为 wss），
   或在局域网内直接使用 `http://`。

## 技术栈

- Tauri 2 外壳（无自定义 Rust 代码）— `docs/adr/0001`
- React + TypeScript + Vite，Tailwind CSS v4
- assistant-ui + shadcn 风格自研组件 — `docs/adr/0002`
- Zustand 状态管理，原生 WebSocket + 自研重连管理器
- i18next（默认 `en`，内置 `zh-CN`）

## 开发

```bash
pnpm install
pnpm dev                # 浏览器开发 http://localhost:5173（最快迭代）
pnpm tauri android dev  # 通过 adb 连接真机/模拟器
pnpm build              # 类型检查 + 打包到 dist/
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

## macOS 应用（自行打包）

```bash
pnpm build:macos          # 未签名 .app -> dist/ZCode-acp.app
```

不提供 macOS 预构建产物——任何人都可以从源码自行打包。构建产物未签名
（无 Developer ID / 公证）：在构建机上直接可用；拷到其他机器需绕过
Gatekeeper（右键 → 打开）。内嵌 Info.plist 放行明文 http://ws:// hub
（与 Android 的 cleartext 策略一致）。

发版（维护者）：`pnpm release <version>` 一步完成版本号更新、APK 构建、
提交、打 tag、推送和发布 GitHub Release。

## 独立 Web 部署

本应用同时是一个纯 SPA —— 未使用任何 Tauri API —— 因此 Web 构建可在 iOS、
桌面及任意浏览器中运行。自行部署即可，无需自建后端：hub 已设置
`Access-Control-Allow-Origin: *`。

**一键部署（Netlify）** —— 会把仓库 fork 到你自己的 GitHub 账户并自动构建：

[![Deploy to Netlify](https://www.netlify.com/img/deploy/button.svg)](https://app.netlify.com/start/deploy?repository=https://github.com/william0wang/zcode-acp-remote)

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

## 文档

- `README.md` — English docs
- `CONTEXT.md` — 术语表
- `docs/adr/` — 架构决策记录

## 许可证

[MIT](LICENSE)
