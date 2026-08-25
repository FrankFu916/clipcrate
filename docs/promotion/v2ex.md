# V2EX 推广文案（分享创造节点）

> 发布地址：https://www.v2ex.com/go/create （分享创造）
> 可同步到：/go/programmer、/go/macOS。建议先在 README 里放演示 GIF 再发。

## 标题

```
clipcrate：终端优先的剪贴板历史管理器，一个静态二进制覆盖 macOS / Linux / Windows
```

## 正文

```markdown
各位好，分享一个我最近写的开源小工具：clipcrate。

GitHub：https://github.com/FrankFu916/clipcrate

## 解决什么问题

市面上的剪贴板历史工具要么是 GUI 应用（Maccy、CopyQ、Ditto），要么是单一平台的
终端工具（clipcat 只支持 X11、cliphist 只支持 Wayland）。我想要的是一个在任何
机器上都能装、且完全不用离开键盘的方案，于是用 Rust 写了这个：

- 后台 watcher 记录所有复制内容（文本 + 截图），一条命令装成登录自启服务
  （launchd / systemd / Windows 注册表，三平台通用）
- `clipcrate pick` 是模糊搜索 TUI，回车后内容直接输出到 stdout 且不带换行，
  所以可以 `cd "$(clipcrate pick)"`、`vim $(clipcrate pick)`
- `pick --copy` 配合 skhd / sxhkd / AutoHotkey 绑一个全局热键就是系统级历史
- 截图按内容哈希去重存 PNG，从选择器里可以直接粘贴回去
- 隐私设计：`clipcrate config deny-add 'ghp_[A-Za-z0-9]{20,}'` 之后，命中正则的
  内容根本不会落盘；存储是本地目录下的纯 JSONL，删掉目录即无痕
- 去重策略可选 bump / update / all，置顶条目永不被 LRU 淘汰

## 一些实现细节

- 单二进制约 1.4 MB，无运行时依赖；watcher 用轮询而不是事件钩子，
  换来四个平台一套代码路径、无需额外权限，空闲时每轮只做一次剪贴板读取
- 存储是 append-only JSONL + flock + 原子重写，CLI 与守护进程可安全并发
- 26 个单元测试 + 12 个端到端 CLI 测试，CI 跑三平台矩阵

## 安装

​```bash
brew install frankfu916/tap/clipcrate
# 或
cargo install clipcrate
​```

求 star、求 issue、求骂（架构和 API 设计上的意见都欢迎）。
下一步计划做事件驱动的监听后端和可选的静态加密，见 README 路线图。
```
