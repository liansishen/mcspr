# MCS Panel — Minecraft 服务端管理面板

基于 **Rust (Axum + Tokio)** 的本地 Minecraft 服务器管理面板。前端资源内嵌进二进制，`cargo build` 后得到单个可执行文件，开箱即用。

## 功能

| 模块 | 说明 |
| --- | --- |
| 📊 仪表盘 | 系统 CPU / 内存使用率，实例运行状态、在线玩家、进程 CPU / 内存占用，快捷启停 |
| 🗂 实例管理 | 新建实例三种方式：**自动下载官方原版服务端**（版本清单 BMCLAPI 镜像回退、SHA1 校验、进度条）、**创建模组服**（Fabric / Quilt 官方一键启动器；Forge / NeoForge 运行官方安装器，自动识别 `@argfile` 启动方式）、导入整合包（ZIP / 本地目录）；启动 / 停止 / 重启 / 删除、异常退出自动重启 |
| 📦 整合包导入 | 上传 ZIP 或指定本地目录；自动解压（去除公共根目录）、应用 CurseForge / Modrinth `overrides`、自动识别主程序 JAR 与 Forge / NeoForge 1.17+ 启动参数（`@argfile`），导入过程带实时日志 |
| 🧩 模组管理 | 解析 Fabric / Quilt / Forge / NeoForge 模组元数据（名称、版本、加载器、MC 版本、作者），启用 / 禁用（`.disabled` 重命名）、上传、删除；**在线下载模组**：Modrinth（无需鉴权）与 CurseForge（面板设置中填 API Key）搜索、按版本/加载器过滤、一键下载到 mods 目录 |
| ⚙️ 服务器设置 | 图形化编辑 `server.properties`（保留注释与顺序，按类型渲染输入框 + 中文说明）；实例设置（Java 路径、JVM 内存、JVM 参数、主程序 JAR 选择） |
| 📁 配置文件 | 文件浏览器：浏览 / 新建文件夹 / 上传 / 重命名 / 删除；文本文件在线编辑（`config/`、`ops.json`、白名单等） |
| 🖥 控制台 | 实时 WebSocket 控制台（着色日志、GBK 自动转码）、发送任意服务器命令、玩家进出自动追踪、EULA 提示与一键同意；**Java 版本不匹配自动给出中文提示** |
| 👥 用户管理 | 控制台页内管理**在线玩家**（一键 OP / 踢出 / 封禁 / 加白名单）与 OP / 白名单 / 封禁玩家 / 封禁 IP：服务器运行中走命令实时生效；未运行直接读写 ops.json / whitelist.json / banned-*.json（自动解析玩家 UUID：本地缓存 → Mojang API → 离线 UUID 回退） |
| ☕ Java 扫描 | 一键扫描本机全部 Java（Program Files 各发行版、Prism / MultiMC / 官方启动器自带 JRE、IntelliJ .jdks、JAVA_HOME、PATH），结果持久化，实例设置中下拉选择 |
| 🎨 三套主题 | 深色 / 亮色 / **MC 像素**（泥土背景、背包灰面板、石质按钮、MC 聊天配色控制台，内置开源像素字体「缝合像素」覆盖中文），侧边栏底部一键循环切换，偏好本地记忆并跟随系统 |
| 🔐 安全 | 可选访问令牌（`config.toml` 中设置 `token` 后 API 与 WebSocket 均需鉴权） |

## 环境要求

- Java（与服务器版本匹配的 JRE/JDK，如 Java 17+ / 21+）
- Rust 1.75+（仅构建时需要）

## 构建与运行

```bash
cargo run --release
```

浏览器打开 **http://127.0.0.1:8080** 即可。

首次运行会在当前目录生成 `config.toml`：

```toml
listen = "127.0.0.1:8080"   # 监听地址，改为 0.0.0.0:8080 可局域网访问
data_dir = "data"           # 实例数据目录
token = ""                  # 访问令牌，留空则无需鉴权；设置后立即生效
```

## 使用流程

1. **导入整合包**：实例管理 → 导入整合包 → 上传 ServerPack 的 ZIP（或填本地已解压目录路径）。导入完成后自动跳到实例控制台。
2. **同意 EULA**：实例页顶部横幅点击「同意 EULA」（或手动确认 `eula.txt`）。
3. **检查设置**：实例设置中确认主程序 JAR、Java 路径、内存分配；点「检测」验证 Java。
4. **启动**：点击 ▶ 启动，控制台实时查看日志；`Done (...)` 出现即启动完成。
5. **日常管理**：模组页管理 mods；文件页直接改 `config/` 下的配置；服务器设置页改 `server.properties`（改完重启生效）。

## 目录结构

```
data/instances/<实例ID>/
├── instance.json        # 实例元数据（面板管理，勿手改）
├── server.jar / 模组启动脚本
├── mods/  config/  world/ ...
└── logs/latest.log      # 控制台重启后自动回填末尾 200 行

web/                     # 前端源码（构建时内嵌进二进制）
└── fonts/               # MC 主题像素字体（Fusion Pixel，OFL 许可）
```

## 说明与已知限制

- **Forge / NeoForge 1.17+**：新版服务包使用 `@libraries/.../win_args.txt` 启动，导入器会自动从 `run.bat` 中提取参数；若包内未安装 libraries，需先运行一次官方安装器。
- 面板进程重启时，正在运行的服务器进程会成为孤儿进程（不会退出），建议先停止实例再重启面板。
- `server.properties` 修改在服务器运行期间不会热生效，需重启实例。
- 大于 2MB 或二进制格式的文件不允许在线编辑（可上传替换）。
- 所有路径操作都限制在实例目录内，拒绝 `..` 与绝对路径。

## API 一览（供脚本调用）

```
GET    /api/stats                        全局统计
GET    /api/versions                     Minecraft 版本清单（官方源 + 镜像回退）
GET    /api/instances                    实例列表
POST   /api/instances                    新建 {name, mc_version?}（带版本则自动下载服务端）
GET    /api/instances/{id}/users         用户列表（ops/whitelist/banned/bannedIps）
POST   /api/instances/{id}/users/action  用户操作 {action, target, reason?}
POST   /api/instances/import/upload      上传 zip 导入（multipart: name, file）
POST   /api/instances/import/path        本地目录导入 {path, name?}
GET    /api/jobs/{id}                    导入任务进度
GET    /api/instances/{id}               实例详情（含状态/玩家/EULA）
PATCH  /api/instances/{id}               修改实例设置
DELETE /api/instances/{id}               停止并删除实例
POST   /api/instances/{id}/start|stop|restart|eula|open
POST   /api/instances/{id}/command       发送控制台命令 {command}
GET    /api/instances/{id}/console?after= 拉取日志（REST 备用）
WS     /api/instances/{id}/ws?token=     实时控制台（收 JSON 日志行 / 发文本命令）
GET    /api/instances/{id}/mods          模组列表
POST   /api/instances/{id}/mods/toggle|delete|upload
GET    /api/instances/{id}/files?path=   目录列表
GET/PUT /api/instances/{id}/files/content 读取/保存文本文件
POST   /api/instances/{id}/files/mkdir|delete|rename|upload
GET/PUT /api/instances/{id}/properties   server.properties 读写
GET/PUT /api/settings                    面板设置
```
