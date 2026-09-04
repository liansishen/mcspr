//! 极简 RCON 客户端（Source RCON 协议），用于 TPS 采样与远程命令
//!
//! 同步实现，调用方用 spawn_blocking 包装。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const SERVERDATA_AUTH: i32 = 3;
const SERVERDATA_EXECCOMMAND: i32 = 2;
const SERVERDATA_RESPONSE_VALUE: i32 = 0;

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
    let len = i32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
    if len < 10 || len > 8192 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("异常包长度: {len}"),
        ));
    }
    let body = read_exact(stream, len)?;
    let id = i32::from_le_bytes(body[0..4].try_into().unwrap());
    let ptype = i32::from_le_bytes(body[4..8].try_into().unwrap());
    Ok((id, ptype, body[8..len - 2].to_vec()))
}

impl RconClient {
    pub fn connect(addr: &str, password: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut client = Self { stream };
        // 登录：原版会先回一个空 RESPONSE_VALUE 再回 AUTH_RESPONSE(id=-1 表示失败)
        write_packet(&mut client.stream, 1, SERVERDATA_AUTH, password.as_bytes())?;
        let _ = read_packet(&mut client.stream);
        let (id, _, _) = read_packet(&mut client.stream)?;
        if id == -1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "RCON 密码错误",
            ));
        }
        Ok(client)
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

use std::path::Path;

/// 解析 Paper / Purpur / Spigot 风格的 tps / mspt 输出
pub fn parse_tps(out: &str) -> Option<(f64, Option<f64>)> {
    let re_tps = regex::Regex::new(r"TPS[^:]*:\s*([\d.]+)").ok()?;
    let tps = re_tps.captures(out)?.get(1)?.as_str().parse().ok()?;
    let mspt = regex::Regex::new(r"MSPT[^:]*:\s*([\d.]+)")
        .ok()
        .and_then(|r| r.captures(out))
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok());
    Some((tps, mspt))
}
