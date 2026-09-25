# clipcrate

**终端优先的剪贴板历史管理器。** 一个极小的后台守护进程记录你复制的所有内容；
模糊搜索选择器一键取回 —— 100% 本地存储，没有你不掌控的进程，无云端、无账号。

> English documentation: [README.md](README.md)

```text
$ clipcrate pick          # 在 TUI 里模糊搜索剪贴板历史
$ clipcrate list          # 按时间列出条目（含大小）
$ clipcrate get - | pbcopy
```

## 为什么再造一个剪贴板管理器？

主流历史管理器几乎都是 GUI 应用（Maccy、CopyQ、Ditto），终端用户却要为一次
粘贴离开键盘。`clipcrate` 从第一天起就是为终端设计的：

- **可组合** — 所有命令都干净地读写管道：`clipcrate get - | jq .`、
  `clipcrate add < file`。
- **天然跨平台** — 单个 Rust 二进制，覆盖 macOS、Linux（X11 和 Wayland）、
  Windows。
- **本地优先的隐私** — 磁盘上是普通 JSONL 文件，原子写入；支持 deny 正则，
  让密钥根本进不了存储（如 `^sk-`）。
- **服务化一条命令** — 自带 launchd / systemd / Windows 注册表自启动安装器；
  也可以自己在 tmux 里跑 `watch`。

## 安装

```bash
# 从源码（Rust 1.88+）
cargo install --git https://github.com/FrankFu916/clipcrate

# 或从 Releases 下载预编译二进制放进 PATH
```

## 快速上手

```bash
clipcrate watch            # 开始记录（Ctrl+C 停止）
clipcrate install-service  # …或交给 launchd/systemd/Windows 登录自启

clipcrate list             # 我复制过什么？
clipcrate pick > file      # 交互选择，重定向到任何地方
clipcrate clear --last     # 手滑了
```

### 选择器快捷键

| 按键              | 功能                        |
| ----------------- | --------------------------- |
| 直接输入          | 模糊搜索                    |
| ↑ / ↓、PgUp/PgDn  | 移动选中                    |
| Enter             | 将选中内容输出到 stdout     |
| Esc / Ctrl+C      | 取消                        |
| Ctrl+P            | 置顶/取消置顶（置顶不过期） |
| Delete            | 删除条目                    |

`pick` 输出不带换行符，因此 `cd "$(clipcrate pick)"` 可以直接用。

### 图片

截图会以 PNG 存储（按内容哈希去重）。`pick` 会把图片写回系统剪贴板；
`get - > shot.png` 导出字节。

## 配置

数据目录（macOS 为 `~/Library/Application Support/clipcrate`，可用
`$CLIPCRATE_HOME` 覆盖）下的 `config.toml`：

```toml
max_entries = 1000      # 未置顶条目上限，LRU 淘汰
poll_ms = 700           # watcher 轮询间隔
min_length = 1
max_length = 1000000
deny_patterns = []      # 命中即永不记录的正则
dedup = "bump"          # bump | update | all
preview_lines = 8
```

让密钥远离历史记录：

```bash
clipcrate config deny-add 'sk-[A-Za-z0-9]{10,}'
clipcrate config deny-add 'ghp_[A-Za-z0-9]{20,}'
```

watcher 运行中会热加载配置改动。

## 数据与隐私

一切都在 `$CLIPCRATE_HOME` 目录里：`history.jsonl`、`config.toml`、`images/`。
永远不联网。删掉目录，就不会留下任何关于你的痕迹。

## 许可

MIT，见 [LICENSE](LICENSE)。
