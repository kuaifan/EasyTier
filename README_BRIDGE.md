# EasyTier TCP Port Bridge

本文档介绍如何在 EasyTier 中启用和使用「端口桥接」（TCP Port Bridge）功能。该功能的目标和 `socat TCP-LISTEN:15037,bind=0.0.0.0,fork,reuseaddr TCP:localhost:5037` 等效：在本地监听一个端口，并把进入的 TCP 连接转发到指定目标。

## 通过 CLI 启动

在执行 `easytier-core` 时增加 `--tcp-bridge` 参数即可。每条规则格式为 `监听地址->目标地址`，多个规则用逗号分隔或重复添加参数。

```bash
# 监听 0.0.0.0:15037，并转发到 127.0.0.1:5037
easytier-core \
  --tcp-bridge 0.0.0.0:15037->127.0.0.1:5037 \
  # 其他参数 ...
```

CLI 支持多条规则，例如：

```bash
easytier-core \
  --tcp-bridge 0.0.0.0:15037->127.0.0.1:5037 \
  --tcp-bridge 0.0.0.0:18000->10.0.0.10:8000
```

## 通过配置文件

如果使用 `config.toml` 等配置文件，可添加 `[[tcp_bridge]]` 块：

```toml
[[tcp_bridge]]
listen = "0.0.0.0:15037"
target = "127.0.0.1:5037"

[[tcp_bridge]]
listen = "0.0.0.0:18000"
target = "10.0.0.10:8000"
```

保存后重启 EasyTier 实例即可生效。
