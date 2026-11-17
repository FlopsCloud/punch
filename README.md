# punch

单二进制的反向 TCP 隧道。客户端主动连出，服务端在公网接受连接，再让客户端去拨它能访问的任意地址。管理页编译进二进制，部署时不带额外文件。

更细的说明见 [docs/guide.html](docs/guide.html)。

## 编译

需要 Rust 1.73 或更新，以及系统的 OpenSSL（1.1.1 或 3，`libssl`）。

```bash
cargo build --release
./target/release/punch --help
```

Linux 上数据面是 epoll + splice。macOS 上是 kqueue，拷贝在用户态。

## 服务端

没有配置文件时，第一次启动会在当前目录生成 `punch-server.yaml` 和一份自签名证书，并在终端打出 api key。

```bash
punch server
punch server -c /var/local/punch/punch-server.yaml
```

默认端口是控制 `0.0.0.0:8024`、数据 `0.0.0.0:8025`、管理页 `0.0.0.0:8443`。监听地址写成 `0.0.0.0` 对外，或 `127.0.0.1` 只本机。`web.cert` / `web.key` 用 nginx 的 PEM（`PRIVATE KEY`、`RSA PRIVATE KEY`、`EC PRIVATE KEY`）。

## 客户端

```bash
punch client --server HOST:8024 --key API_KEY --name edge \
  --map 0.0.0.0:8080=10.0.0.8:80 \
  --map 127.0.0.1:2222=192.168.1.9:22
```

`--map` 可以重复。等号左边是服务端要监听的 `ip:port`，右边是客户端能访问的 `host:port`。`--name` 省略时用主机名。同名新连接会替换旧会话。

映射也可以从服务端开：

```bash
curl -k -H "X-Api-Key: API_KEY" -H "Content-Type: application/json" \
  -d '{"client_name":"edge","bind":"0.0.0.0:18080","target":"10.0.0.8:80"}' \
  https://HOST:8443/api/mappings
```

客户端拨目标被拒绝时，映射仍保留，管理页和 `/api/events` 里会出现 `listen failed`。

## 管理页

`https://<主机>:8443/`，用配置里的 api key 登录。`/healthz` 不需要密钥。`/api/` 需要 `X-Api-Key` 或 `Authorization: Bearer <key>`。

要加一个接口，在 `src/admin.rs` 写处理函数，再在 `routes()` 里加一行。

## 压测

```bash
punch client --server HOST:8024 --key API_KEY \
  --bench 127.0.0.1:19090 --expose-bench 0.0.0.0:18080

wrk -t4 -c256 -d30s http://HOST:18080/bench?size=1048576
```

`GET /bench?size=N` 吐出 N 字节，`POST /bench` 回报读到的字节数。

## 部署

`deploy.sh` 把当前目录 rsync 到服务器，在服务器上编译，并重启 systemd 服务。已有的 `punch-server.yaml` 和 `certs/` 不会被覆盖。

主机和端口不要写进仓库。放在环境变量里，或写在 `deploy.local.env`（这个文件已被忽略）：

```bash
# deploy.local.env
PUNCH_HOST=user@host
```

```bash
./deploy.sh
```

换端口：

```bash
PUNCH_HOST=user@host \
PUNCH_CONTROL_PORT=8024 PUNCH_DATA_PORT=8025 PUNCH_WEB_PORT=8443 \
./deploy.sh
```
