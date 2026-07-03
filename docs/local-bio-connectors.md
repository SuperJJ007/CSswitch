# 本地 bio connectors

Claude Science 的 Connectors Directory 里有一组 HCLS（health and life sciences，健康与生命科学）远程连接器，例如 PubMed、Clinical Trials、ChEMBL、bioRxiv。它们在官方环境里由 Anthropic 托管，虚拟登录沙箱没有真实 claude.ai 组织会话，所以这些 hosted connectors（官方托管连接器）会显示 `Failed`。

这不代表所有 MCP（Model Context Protocol，模型工具协议）都不可用。Claude Science runtime（运行时）里同时带有本地 `bio-tools` stdio（标准输入输出，本机进程通信）实现。CSSwitch 可以把这四个远程连接器补成显式的本地连接器：

- `pubmed-local`
- `clinical-trials-local`
- `chembl-local`
- `biorxiv-local`

## 一键安装

先用 CSSwitch 启动 Claude Science 沙箱，然后运行：

```bash
scripts/install-local-bio-connectors.sh
```

可选参数：

```bash
scripts/install-local-bio-connectors.sh \
  --data-dir "$HOME/.csswitch/sandbox/home/.claude-science" \
  --port 8990 \
  --agent OPERON
```

脚本会幂等执行（重复运行不会重复创建）：

1. 通过本地 single-use URL（一次性登录链接）换取 daemon（守护进程）API cookie。
2. 调用 `/api/mcp-servers/local` 注册四个 local-stdio 连接器。
3. 把本地连接器挂到目标 agent（代理，默认 `OPERON`）。
4. 从该 agent 卸载对应的 `bundled:*` 官方远端连接器，避免工具路由碰到失败的 hosted connector。

脚本只会写 CSSwitch 沙箱 data-dir，默认是：

```text
~/.csswitch/sandbox/home/.claude-science
```

它拒绝真实目录：

```text
~/.claude-science
```

## 预期 UI

运行后，Directory（官方连接器目录）里的 PubMed、Clinical Trials、ChEMBL、bioRxiv 仍可能显示 `Failed`。这是官方远端目录项的状态，不是本地替身的状态。

真正给 `OPERON` 使用的是 `*-local` 连接器。脚本末尾会验证这些本地连接器已经 connected（已连接）并且工具数量非零。

## 性能与限制

本地 stdio 连接器会在本机启动 Python MCP 进程。空闲压力较低，实际负载主要来自搜索、JSON/XML 解析和全文抓取。大批量全文下载或多个会话并发调研时，会增加 CPU、内存和网络压力。

这些本地连接器复用 Claude Science runtime 自带的工具代码和 schema（工具接口定义）。脚本只改变连接器注册与 agent 挂载，不改工具函数本身。
