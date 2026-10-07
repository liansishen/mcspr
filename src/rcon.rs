//! 极简 RCON 客户端（Source RCON 协议），用于 TPS 采样与远程命令
//!
//! 同步实现，调用方用 spawn_blocking 包装。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

const SERVERDATA_AUTH: i32 = 3;
const SERVERDATA_EXECCOMMAND: i32 = 2;
const SERVERDATA_RESPONSE_VALUE: i32 = 0;

/// 单个响应包的合理上限（`forge tps` 在 GTNH 上百个维度时会输出现象级文本）
const MAX_PACKET: usize = 1 << 20;

/// 按优先顺序尝试的 TPS 查询命令：不同服务端/模组组合只有其中一个可用
const TPS_COMMANDS: &[&str] = &["forge tps", "cofh tps", "tps", "mspt"];

pub struct RconClient {
    stream: TcpStream,
}

fn write_packet(stream: &mut TcpStream, id: i32, ptype: i32, body: &[u8]) -> std::io::Result<()> {
    let len = (4 + 4 + body.len() + 2) as i32;
    let mut buf = Vec::with_capacity(len as usize + 4);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&id.to_le_bytes());
    buf.extend_from_slice(&ptype.to_le_bytes());
    buf.extend_from_slice(body);
    buf.extend_from_slice(&[0, 0]);
    stream.write_all(&buf)
}

fn read_exact(stream: &mut TcpStream, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_packet(stream: &mut TcpStream) -> std::io::Result<(i32, i32, Vec<u8>)> {
    let len_bytes = read_exact(stream, 4)?;
    let len = i32::from_le_bytes(len_bytes.try_into().unwrap());
    if len < 10 || len as usize > MAX_PACKET {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("异常包长度: {len}"),
        ));
    }
    let len = len as usize;
    let body = read_exact(stream, len)?;
    let id = i32::from_le_bytes(body[0..4].try_into().unwrap());
    let ptype = i32::from_le_bytes(body[4..8].try_into().unwrap());
    Ok((id, ptype, body[8..len - 2].to_vec()))
}

/// 已有连接在服务端重启或空闲超时后会失效，此时重建一次
pub struct Session {
    addr: String,
    client: RconClient,
    /// 已验证可解析出 TPS 的命令，后续采样直接复用，避免每轮试探多个命令
    tps_cmd: Option<&'static str>,
}

impl RconClient {
    pub fn connect(addr: &str, password: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut client = Self { stream };
        write_packet(&mut client.stream, 1, SERVERDATA_AUTH, password.as_bytes())?;
        // 1.7.10 只回一个包（成功 id=1，密码错误 id=-1）；较新实现会先回一个空的
        // RESPONSE_VALUE 再回鉴权结果。这里读一个包判定结果，再短暂尝试排干多余的包，
        // 避免固定等待第二个包导致每次采样都超时失败。
        let (id, _, _) = read_packet(&mut client.stream)?;
        if id == -1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "RCON 密码错误",
            ));
        }
        // 较新实现会先回一个空包、再回真正的鉴权结果，这里补读一次以识别其中的失败结果
        if let Some(id) = client.drain_pending() {
            if id == -1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "RCON 密码错误",
                ));
            }
        }
        Ok(client)
    }

    /// 读取已到达的后续包；返回其中出现的负数 id（-1 表示鉴权失败）
    fn drain_pending(&mut self) -> Option<i32> {
        let _ = self
            .stream
            .set_read_timeout(Some(Duration::from_millis(120)));
        let mut auth_failure = None;
        loop {
            match read_packet(&mut self.stream) {
                Ok((id, _, _)) => {
                    if id < 0 {
                        auth_failure = Some(id);
                    }
                }
                Err(_) => break,
            }
        }
        let _ = self.stream.set_read_timeout(Some(Duration::from_secs(3)));
        auth_failure
    }

    /// 执行命令并返回输出。
    ///
    /// 只发送命令包并读取响应：不能用“追加一个空命令包、读到它的响应为止”的经典做法，
    /// 1.7.10 执行空命令后会把连接关掉，导致每次采样都得重连（服务端日志被连接记录刷屏）。
    pub fn command(&mut self, cmd: &str) -> std::io::Result<String> {
        write_packet(&mut self.stream, 2, SERVERDATA_EXECCOMMAND, cmd.as_bytes())?;
        let mut out = String::new();
        let (_, ptype, body) = read_packet(&mut self.stream)?;
        if ptype == SERVERDATA_RESPONSE_VALUE && !body.is_empty() {
            out.push_str(&String::from_utf8_lossy(&body));
        }
        // 响应可能在 4096 字节处分包，短期内继续读取并拼接
        let _ = self
            .stream
            .set_read_timeout(Some(Duration::from_millis(120)));
        loop {
            match read_packet(&mut self.stream) {
                Ok((_, ptype, body)) => {
                    if ptype == SERVERDATA_RESPONSE_VALUE && !body.is_empty() {
                        out.push_str(&String::from_utf8_lossy(&body));
                    }
                }
                Err(_) => break,
            }
        }
        let _ = self.stream.set_read_timeout(Some(Duration::from_secs(3)));
        Ok(out)
    }
}

/// 从实例目录的 server.properties 解析 RCON 配置
pub fn rcon_config(instance_dir: &Path) -> Option<(String, String, String)> {
    // (addr, password, port)
    let text = std::fs::read_to_string(instance_dir.join("server.properties")).ok()?;
    let mut enable = false;
    let mut port = String::from("25575");
    let mut password = String::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "enable-rcon" => enable = v.trim().eq_ignore_ascii_case("true"),
                "rcon.port" => port = v.trim().to_string(),
                "rcon.password" => password = v.trim().to_string(),
                _ => {}
            }
        }
    }
    if !enable || password.is_empty() {
        return None;
    }
    Some((format!("127.0.0.1:{port}"), password, port))
}

/// 复用实例的 RCON 连接，避免每轮采样新建连接（服务端会为每次连接打印一行日志）
fn ensure(slot: &Mutex<Option<Session>>, dir: &Path) -> Result<(), String> {
    let (addr, password, _) = rcon_config(dir).ok_or_else(|| "未启用 RCON".to_string())?;
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    if guard.as_ref().map(|s| s.addr != addr).unwrap_or(true) {
        *guard = None;
    }
    if guard.is_none() {
        let client = RconClient::connect(&addr, &password).map_err(|e| e.to_string())?;
        *guard = Some(Session {
            addr,
            client,
            tps_cmd: None,
        });
    }
    Ok(())
}

fn run(slot: &Mutex<Option<Session>>, cmd: &str) -> Result<String, String> {
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .as_mut()
        .ok_or_else(|| "RCON 连接未建立".to_string())?
        .client
        .command(cmd)
        .map_err(|e| e.to_string())
}

/// 执行一条命令（不重试）。用户触发的命令走这里，避免连接抖动导致命令被执行两次
pub fn exec(slot: &Mutex<Option<Session>>, dir: &Path, cmd: &str) -> Result<String, String> {
    ensure(slot, dir)?;
    run(slot, cmd)
}

/// 执行一条只读命令；连接已失效时重建并重试一次
fn exec_retry(slot: &Mutex<Option<Session>>, dir: &Path, cmd: &str) -> Result<String, String> {
    match exec(slot, dir, cmd) {
        Ok(out) => Ok(out),
        Err(first) => {
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
            ensure(slot, dir)?;
            run(slot, cmd).map_err(|e| format!("{first}；重连后仍失败：{e}"))
        }
    }
}

/// 采样 TPS/MSPT：优先复用已验证的命令，其余按顺序试探
pub fn sample_tps(slot: &Mutex<Option<Session>>, dir: &Path) -> Result<(f64, Option<f64>), String> {
    let known = slot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|s| s.tps_cmd);
    let mut last_error = String::new();
    for cmd in TPS_COMMANDS
        .iter()
        .copied()
        .filter(|c| Some(*c) == known)
        .chain(TPS_COMMANDS.iter().copied().filter(|c| Some(*c) != known))
    {
        match exec_retry(slot, dir, cmd) {
            Ok(out) => {
                if let Some(value) = parse_tps(&out) {
                    if let Some(s) = slot.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                        s.tps_cmd = Some(cmd);
                    }
                    return Ok(value);
                }
            }
            Err(e) => last_error = e,
        }
    }
    Err(if last_error.is_empty() {
        "该服务端没有可用的 TPS 查询命令".to_string()
    } else {
        last_error
    })
}

/// 解析 TPS 输出：支持 Paper/Spigot 的 `TPS: x` / `MSPT: y`、Forge 的
/// `Mean TPS: x` + `Mean tick time: y ms`，以及 COFH 的 `x TPS/y MS`
pub fn parse_tps(out: &str) -> Option<(f64, Option<f64>)> {
    // `TPS: 20.0`、`Mean TPS: 20.000`（冒号在 TPS 之后）
    let tps = regex::Regex::new(r"TPS[^:]*:\s*([\d.]+)")
        .ok()
        .and_then(|r| r.captures(out))
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
        // `20.00 TPS/0.90MS`（数字在 TPS 之前）
        .or_else(|| {
            regex::Regex::new(r"([\d.]+)\s*TPS")
                .ok()
                .and_then(|r| r.captures(out))
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
        })?;
    let mspt = regex::Regex::new(r"(?i)MSPT[^:]*:\s*([\d.]+)")
        .ok()
        .and_then(|r| r.captures(out))
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
        .or_else(|| {
            regex::Regex::new(r"(?i)Mean tick time[^:]*:\s*([\d.]+)")
                .ok()
                .and_then(|r| r.captures(out))
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
        })
        .or_else(|| {
            regex::Regex::new(r"(?i)([\d.]+)\s*MS\b")
                .ok()
                .and_then(|r| r.captures(out))
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
        });
    Some((tps, mspt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    const TPS_TEXT: &str = "Dim 0 : Mean tick time: 1.250 ms. Mean TPS: 19.980";

    #[derive(Clone, Copy)]
    struct ServerStyle {
        /// 较新实现：鉴权时先回一个空包，再回真正的结果
        two_packet_auth: bool,
        auth_ok: bool,
        /// 把一条响应拆成几个分片（真实协议会在 4096 字节处分片）
        fragments: usize,
    }

    fn style(two_packet_auth: bool, auth_ok: bool, fragments: usize) -> ServerStyle {
        ServerStyle {
            two_packet_auth,
            auth_ok,
            fragments,
        }
    }

    fn server_read(sock: &mut TcpStream) -> std::io::Result<(i32, i32, String)> {
        let mut len = [0u8; 4];
        sock.read_exact(&mut len)?;
        let len = i32::from_le_bytes(len) as usize;
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body)?;
        let id = i32::from_le_bytes(body[0..4].try_into().unwrap());
        let ptype = i32::from_le_bytes(body[4..8].try_into().unwrap());
        Ok((
            id,
            ptype,
            String::from_utf8_lossy(&body[8..len - 2]).to_string(),
        ))
    }

    fn server_send(sock: &mut TcpStream, id: i32, body: &str) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&SERVERDATA_RESPONSE_VALUE.to_le_bytes());
        payload.extend_from_slice(body.as_bytes());
        payload.extend_from_slice(&[0, 0]);
        let mut out = (payload.len() as i32).to_le_bytes().to_vec();
        out.extend_from_slice(&payload);
        sock.write_all(&out).unwrap();
    }

    /// 最小 RCON 服务端，记录每条连接上执行过的命令
    struct Fake {
        addr: String,
        stop: Arc<AtomicBool>,
        connections: Arc<StdMutex<Vec<Arc<StdMutex<Vec<String>>>>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Fake {
        fn start(style: ServerStyle) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let stop = Arc::new(AtomicBool::new(false));
            let connections: Arc<StdMutex<Vec<Arc<StdMutex<Vec<String>>>>>> =
                Arc::new(StdMutex::new(Vec::new()));
            let (stop2, store) = (stop.clone(), connections.clone());
            let handle = std::thread::spawn(move || {
                while !stop2.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut sock, _)) => {
                            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                            let executed = Arc::new(StdMutex::new(Vec::new()));
                            store.lock().unwrap().push(executed.clone());
                            loop {
                                let Ok((id, ptype, body)) = server_read(&mut sock) else {
                                    break;
                                };
                                if ptype == SERVERDATA_AUTH {
                                    let ok = style.auth_ok && body == "good-password";
                                    if style.two_packet_auth {
                                        server_send(&mut sock, 1, "");
                                    }
                                    server_send(&mut sock, if ok { id } else { -1 }, "");
                                    if !ok {
                                        break;
                                    }
                                    continue;
                                }
                                executed.lock().unwrap().push(body.clone());
                                let text = if body == "forge tps" {
                                    TPS_TEXT.to_string()
                                } else {
                                    format!("echo:{body}")
                                };
                                let parts = style.fragments.max(1);
                                let size = text.len().div_ceil(parts).max(1);
                                for chunk in text.as_bytes().chunks(size) {
                                    server_send(&mut sock, id, &String::from_utf8_lossy(chunk));
                                }
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                addr,
                stop,
                connections,
                handle: Some(handle),
            }
        }

        fn connection_count(&self) -> usize {
            self.connections.lock().unwrap().len()
        }

        fn commands(&self) -> Vec<String> {
            self.connections
                .lock()
                .unwrap()
                .iter()
                .flat_map(|c| c.lock().unwrap().clone())
                .collect()
        }

        fn instance_dir(&self, tag: &str) -> std::path::PathBuf {
            let dir =
                std::env::temp_dir().join(format!("mcspr-rcon-{tag}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let port = self.addr.rsplit(':').next().unwrap();
            std::fs::write(
                dir.join("server.properties"),
                format!("enable-rcon=true\nrcon.password=good-password\nrcon.port={port}\n"),
            )
            .unwrap();
            dir
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn keeps_one_connection_for_repeated_commands() {
        let server = Fake::start(style(false, true, 1));
        let mut client = RconClient::connect(&server.addr, "good-password").unwrap();
        assert_eq!(client.command("list").unwrap(), "echo:list");
        assert_eq!(client.command("forge tps").unwrap(), TPS_TEXT);
        assert_eq!(server.connection_count(), 1);
        assert_eq!(
            server.commands(),
            vec!["list".to_string(), "forge tps".to_string()]
        );
    }

    #[test]
    fn authenticates_against_two_packet_server() {
        let server = Fake::start(style(true, true, 1));
        let mut client = RconClient::connect(&server.addr, "good-password").unwrap();
        assert_eq!(client.command("list").unwrap(), "echo:list");
        assert_eq!(server.connection_count(), 1);
    }

    #[test]
    fn rejects_wrong_password_on_both_auth_styles() {
        for two_packet in [false, true] {
            let server = Fake::start(style(two_packet, false, 1));
            let err = match RconClient::connect(&server.addr, "wrong") {
                Ok(_) => panic!("错误密码必须连接失败（two_packet={two_packet}）"),
                Err(e) => e,
            };
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn joins_fragmented_responses() {
        let server = Fake::start(style(false, true, 3));
        let mut client = RconClient::connect(&server.addr, "good-password").unwrap();
        assert_eq!(client.command("forge tps").unwrap(), TPS_TEXT);
    }

    #[test]
    fn parses_forge_cofh_and_paper_tps_output() {
        assert_eq!(
            parse_tps("Dim 94 : Mean tick time: 0.082 ms. Mean TPS: 20.000"),
            Some((20.0, Some(0.082)))
        );
        assert_eq!(
            parse_tps("Overall: 20.00 TPS/0.90MS (100%)"),
            Some((20.0, Some(0.9)))
        );
        assert_eq!(
            parse_tps("TPS from last 1m, 5m, 15m: 19.9, 20.0, 20.0\nMSPT: 1.35"),
            Some((19.9, Some(1.35)))
        );
        assert_eq!(
            parse_tps("You must specify which player you wish to perform this action on."),
            None
        );
    }

    #[test]
    fn rcon_config_requires_enable_and_password() {
        let dir = std::env::temp_dir().join(format!("mcspr-rcon-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let props = dir.join("server.properties");
        std::fs::write(
            &props,
            "enable-rcon=false\nrcon.password=abc\nrcon.port=25575\n",
        )
        .unwrap();
        assert!(rcon_config(&dir).is_none());
        std::fs::write(
            &props,
            "enable-rcon=true\nrcon.password=\nrcon.port=25575\n",
        )
        .unwrap();
        assert!(rcon_config(&dir).is_none());
        std::fs::write(
            &props,
            "enable-rcon=true\nrcon.password=abc\nrcon.port=25576\n",
        )
        .unwrap();
        assert_eq!(
            rcon_config(&dir),
            Some((
                "127.0.0.1:25576".to_string(),
                "abc".to_string(),
                "25576".to_string()
            ))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sample_tps_reuses_the_connection_and_caches_the_working_command() {
        let server = Fake::start(style(false, true, 1));
        let dir = server.instance_dir("sample");
        let slot = Mutex::new(None);
        assert_eq!(sample_tps(&slot, &dir), Ok((19.98, Some(1.25))));
        assert_eq!(sample_tps(&slot, &dir), Ok((19.98, Some(1.25))));
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().tps_cmd,
            Some("forge tps")
        );
        // 首个命令即可解析时不再试探其他命令，两次采样复用同一条连接
        assert_eq!(server.connection_count(), 1);
        assert_eq!(
            server.commands(),
            vec!["forge tps".to_string(), "forge tps".to_string()]
        );
        drop(slot);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
