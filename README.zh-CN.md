# Ferryman

[English](README.md) | 简体中文 | [日本語](README.ja.md)

**Ferryman（渡船工）** 是一款多格式文档翻译工具，通过任意 OpenAI 兼容的 Chat API 生成**双语对照**或**仅译文**文档。项目同时提供独立的命令行工具（`ferryman`）和单镜像 Web 应用（`ferryman-web`），支持 Docker 自托管与懒猫微服部署。

> [!NOTE]
> 详细技术文档（[`docs/`](docs/cli.md)）以英文维护作为唯一事实源。

## 核心特性

- **九种文档格式**：支持 EPUB、DOCX、PDF、Markdown、纯文本、SRT、VTT、ASS/SSA 与 LRC，尽量保留原始排版、样式与非文本资源。
- **任意 OpenAI 兼容 API**：可连接云端或自托管的 Chat Completions 接口，内置通用翻译适配器（`generic`）和 Index-Translate 专用适配器（`index-translate`），无需本地 GPU 或内置推理引擎。
- **可中断、可续译**：支持内容寻址翻译缓存、Web 任务私有断点（checkpoint）、上下文溢出自动折半拆分，以及中断后保留部分译文结果。
- **单镜像 Web 工作台**：Rust 后端直接托管 Vue 3 前端，内置 SQLite 任务队列、Server-Sent Events（SSE）实时进度推送、多用户存储隔离与可选的 API Key 静态加密。
- **可选懒猫微服集成**：通过 `--features lazycat` 编译启用，支持基于微服应用委托的模型自动发现，以及用户文稿和挂载网盘目录的直接读写。

## 支持格式

| 格式 | 扩展名 | 双语模式（`bilingual`） | 替换模式（`replace`） | 翻译策略 |
| --- | --- | --- | --- | --- |
| EPUB | `.epub` | 插入 `.hy-zh` 译文块 | 原地替换文本并保留内联标记 | 逐段独立 |
| DOCX | `.docx` | 原段后追加译文段落 | 跨文本 run 原地替换并保留样式 | 逐段独立 |
| PDF | `.pdf` | 逐页插入镜像排版译文页 | 在原段落区域覆盖重绘译文 | 逐段独立 |
| Markdown | `.md`, `.markdown` | 复制完整译文块 | 原地替换文本节点 | 上下文分批 |
| 纯文本 | `.txt` | 原文行后追加译文行 | 逐行替换 | 上下文分批 |
| SubRip / WebVTT | `.srt`, `.vtt` | 同时间轴内原文 + 译文 | 替换字幕文本 | 上下文分批 |
| ASS / SSA | `.ass`, `.ssa` | 同条目内以 `\N` 换行叠加 | 替换对话文本 | 上下文分批 |
| LRC 歌词 | `.lrc` | 复用同时间戳追加译文行 | 替换歌词文本 | 上下文分批 |

格式处理细节、PDF 字体配置、压缩包安全上限与已知限制请查阅 [docs/formats.md](docs/formats.md)。

## 快速开始

### 1. 命令行（CLI）

从源码编译（Rust 工具链版本见 [`rust-toolchain.toml`](rust-toolchain.toml)）：

```bash
cargo build --release --bin ferryman
```

翻译单个文件或递归翻译整个目录：

```bash
# 连接通用 OpenAI 兼容接口
export FERRYMAN_API_KEY="sk-..."
./target/release/ferryman \
  -i book.epub -o book.bilingual.epub \
  --endpoint https://api.openai.com/v1 \
  --model gpt-4o-mini \
  --target 中文

# 连接本地部署的 Index-Translate 服务
./target/release/ferryman \
  -i ./books --mode replace --target English \
  --endpoint http://127.0.0.1:8088/v1 \
  --model index-translate-2b \
  --adapter index-translate
```

完整参数表、输出命名规则、缓存与容错策略见 [docs/cli.md](docs/cli.md)。

### 2. Standalone Docker（Web 应用）

```bash
cp .env.example .env
docker compose -f docker-compose.standalone.yml up -d --build
```

在浏览器打开 `http://127.0.0.1:8080`，点击 **模型与 API** 添加外部模型服务，即可上传文件或选择挂载目录中的文档进行翻译。

> [!NOTE]
> Standalone Docker 默认以单用户模式运行（`FERRYMAN_ALLOW_LOCAL_USER=true`）。若需多用户访问或暴露到公网，请在前端部署鉴权反向代理并注入 `safe_uid` 请求头。详见 [docs/deployment.md](docs/deployment.md)。

### 3. 懒猫微服

懒猫微服交付单个 LPK 包，构建脚本会自动启用 `lazycat` feature 以支持委托模型发现与微服文档/网盘挂载：

```bash
bun install --frozen-lockfile
bun --bun lzc-cli project lint .
bun --bun lzc-cli project release -o dist/ferryman-web.lpk
```

构建前置依赖、本地 Buildx 打包、产物校验与模型发现授权说明见 [docs/packaging.md](docs/packaging.md)。

## 文档导航

| 文档（英文） | 内容范围 |
| --- | --- |
| [docs/cli.md](docs/cli.md) | CLI 完整参数、提示词适配器、输出路径、缓存与中断恢复 |
| [docs/formats.md](docs/formats.md) | 9 种格式能力矩阵、EPUB/DOCX/PDF/Markdown/字幕规则、字体与限制 |
| [docs/deployment.md](docs/deployment.md) | Docker 部署、反代多用户鉴权、完整环境变量表、存储布局与运维 |
| [docs/packaging.md](docs/packaging.md) | 懒猫微服 LPK 打包、验收脚本、挂载权限与模型发现调试 |
| [docs/development.md](docs/development.md) | Workspace 架构、本地联调、`ts-rs` 类型契约、API 路由、测试与依赖策略 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录与迁移说明 |

## 开源许可

Ferryman 采用 **LGPL-3.0-or-later** 许可协议。详见 [COPYING.LESSER](COPYING.LESSER)（LGPL-3.0 全文）与 [COPYING](COPYING)（所引用的 GPL-3.0 全文）。
