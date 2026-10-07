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
        client.drain_pending();
        Ok(client)
    }

    /// 排干紧随鉴权响应之后可能存在的空包（最多等 120ms）
    fn drain_pending(&mut self) {
        let _ = self
            .stream
            .set_read_timeout(Some(Duration::from_millis(120)));
        loop {
            match read_packet(&mut self.stream) {
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let _ = self.stream.set_read_timeout(Some(Duration::from_secs(3)));
    }

    /// 执行命令并返回输出。经典技巧：追加一个 id=99 的哑包，读到它为止
    pub fn command(&mut self, cmd: &str) -> std::io::Result<String> {
        write_packet(&mut self.stream, 2, SERVERDATA_EXECCOMMAND, cmd.as_bytes())?;
        write_packet(&mut self.stream, 99, SERVERDATA_EXECCOMMAND, b"")?;
        let mut out = String::new();
        loop {
            let (id, ptype, body) = read_packet(&mut self.stream)?;
            if id == 99 {
                break;
            }
            if ptype == SERVERDATA_RESPONSE_VALUE && !body.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&String::from_utf8_lossy(&body));
            }
        }
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

    /// 最小 RCON 服务端：`two_packets` 为真时模拟较新实现（先回空包再回鉴权结果）
    fn fake_server(two_packets: bool) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut commands = Vec::new();
            let read_packet = |sock: &mut TcpStream| -> std::io::Result<(i32, i32, String)> {
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
            };
            let send = |sock: &mut TcpStream, id: i32, body: &str| {
                let mut payload = Vec::new();
                payload.extend_from_slice(&id.to_le_bytes());
                payload.extend_from_slice(&SERVERDATA_RESPONSE_VALUE.to_le_bytes());
                payload.extend_from_slice(body.as_bytes());
                payload.extend_from_slice(&[0, 0]);
                let mut out = ((payload.len()) as i32).to_le_bytes().to_vec();
                out.extend_from_slice(&payload);
                sock.write_all(&out).unwrap();
            };
            loop {
                let Ok((id, ptype, body)) = read_packet(&mut sock) else {
                    break;
                };
                if ptype == SERVERDATA_AUTH {
                    if body == "good-password" {
                        if two_packets {
                            // 较新实现：先回一个 id=1 的空 RESPONSE_VALUE
                            send(&mut sock, 1, "");
                        }
                        send(&mut sock, id, "");
                    } else {
                        send(&mut sock, -1, "");
                    }
                    continue;
                }
                commands.push(body.clone());
                if body == "forge tps" {
                    send(
                        &mut sock,
                        id,
                        "Dim 0 : Mean tick time: 1.250 ms. Mean TPS: 19.980",
                    );
                } else {
                    send(&mut sock, id, &format!("echo:{body}"));
                }
            }
            commands
        });
        (addr, handle)
    }

    #[test]
    fn authenticates_against_single_packet_server() {
        let (addr, handle) = fake_server(false);
        let mut client = RconClient::connect(&addr, "good-password").unwrap();
        assert_eq!(
            client.command("forge tps").unwrap(),
            "Dim 0 : Mean tick time: 1.250 ms. Mean TPS: 19.980"
        );
        drop(client);
        let commands = handle.join().unwrap();
        assert_eq!(commands.first().map(String::as_str), Some("forge tps"));
    }

    #[test]
    fn authenticates_against_two_packet_server() {
        let (addr, handle) = fake_server(true);
        let mut client = RconClient::connect(&addr, "good-password").unwrap();
        assert_eq!(client.command("list").unwrap(), "echo:list");
        drop(client);
        let commands = handle.join().unwrap();
        assert_eq!(commands.first().map(String::as_str), Some("list"));
    }

    #[test]
    fn rejects_wrong_password_without_hanging() {
        let (addr, handle) = fake_server(false);
        let err = match RconClient::connect(&addr, "wrong") {
            Ok(_) => panic!("错误密码必须连接失败"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        let _ = handle.join();
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
    fn sample_tps_reuses_the_slot_and_caches_the_working_command() {
        let (addr, handle) = fake_server(false);
        let dir = std::env::temp_dir().join(format!("mcspr-rcon-sample-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let port = addr.rsplit(':').next().unwrap();
        std::fs::write(
            dir.join("server.properties"),
            format!("enable-rcon=true\nrcon.password=good-password\nrcon.port={port}\n"),
        )
        .unwrap();
        let slot = Mutex::new(None);
        // 第一次采样会试探命令，命中 forge tps；第二次直接复用缓存命令与连接
        assert_eq!(sample_tps(&slot, &dir), Ok((19.98, Some(1.25))));
        assert_eq!(sample_tps(&slot, &dir), Ok((19.98, Some(1.25))));
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().tps_cmd,
            Some("forge tps")
        );
        drop(slot);
        let commands = handle.join().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|c| c.as_str() == "forge tps")
                .count(),
            2
        );
        // 首个命令即可解析出 TPS 时，不再继续试探后续命令
        assert_eq!(
            commands.iter().filter(|c| c.as_str() == "cofh tps").count(),
            0
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
