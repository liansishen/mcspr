//! 模组市场：Modrinth（无需鉴权）与 CurseForge（需用户自己的 API Key）

use crate::state::AppState;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct ModSearchItem {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub summary: String,
    pub downloads: u64,
    pub author: String,
    pub icon: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModDep {
    pub project_id: String,
    pub dependency_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModVersionItem {
    pub id: String,
    pub name: String,
    pub filename: String,
    pub url: String,
    pub date: String,
    pub sha1: String,
    pub environment: String,
    pub dependencies: Vec<ModDep>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModSideInfo {
    pub id: String,
    pub client_side: String,
    pub server_side: String,
}

fn http(state: &AppState) -> &reqwest::Client {
    &state.http
}

pub(crate) async fn cf_key(state: &AppState) -> Result<String, String> {
    let key = state.config.read().await.curseforge_api_key.clone();
    if key.trim().is_empty() {
        return Err("CurseForge 需要API Key：请在「面板设置」中填写（console.curseforge.com 可免费创建），或改用 Modrinth".into());
    }
    Ok(key.trim().to_string())
}

fn cf_loader_id(loader: &str) -> Option<i64> {
    match loader {
        "forge" => Some(1),
        "fabric" => Some(4),
        "quilt" => Some(5),
        "neoforge" => Some(6),
        _ => None,
    }
}

/// 搜索模组
pub async fn search(
    state: &AppState,
    source: &str,
    q: &str,
    game: &str,
    loader: &str,
) -> Result<Vec<ModSearchItem>, String> {
    match source {
        "modrinth" => {
            let mut facets = vec![json!(["project_type:mod"])];
            if !game.is_empty() {
                facets.push(json!([format!("versions:{game}")]));
            }
            if !loader.is_empty() {
                facets.push(json!([format!("categories:{loader}")]));
            }
            let facets = serde_json::to_string(&json!(facets)).map_err(|e| e.to_string())?;
            let req = http(state)
                .get("https://api.modrinth.com/v2/search")
                .header("user-agent", "MCS-Panel/0.1")
                .query(&[("limit", "20"), ("index", "downloads"), ("query", q), ("facets", facets.as_str())])
                .timeout(Duration::from_secs(20));
            let v = send_json(req).await?;
            let mut out = Vec::new();
            if let Some(hits) = v.get("hits").and_then(|x| x.as_array()) {
                for h in hits {
                    out.push(ModSearchItem {
                        id: h.get("project_id").and_then(|x| x.as_str()).unwrap_or("").into(),
                        slug: h.get("slug").and_then(|x| x.as_str()).unwrap_or("").into(),
                        name: h.get("title").and_then(|x| x.as_str()).unwrap_or("").into(),
                        summary: h.get("description").and_then(|x| x.as_str()).unwrap_or("").into(),
                        downloads: h.get("downloads").and_then(|x| x.as_u64()).unwrap_or(0),
                        author: h.get("author").and_then(|x| x.as_str()).unwrap_or("").into(),
                        icon: h.get("icon_url").and_then(|x| x.as_str()).unwrap_or("").into(),
                        source: "modrinth".into(),
                    });
                }
            }
            Ok(out)
        }
        "curseforge" => {
            let key = cf_key(state).await?;
            let mut params = vec![
                ("gameId", "432".to_string()),
                ("classId", "6".to_string()),
                ("sortField", "2".to_string()),
                ("sortOrder", "desc".to_string()),
                ("pageSize", "20".to_string()),
            ];
            if !q.is_empty() {
                params.push(("searchFilter", q.to_string()));
            }
            if !game.is_empty() {
                params.push(("gameVersion", game.to_string()));
            }
            if let Some(n) = cf_loader_id(loader) {
                params.push(("modLoaderType", n.to_string()));
            }
            let req = http(state)
                .get("https://api.curseforge.com/v1/mods/search")
                .header("x-api-key", &key)
                .query(&params)
                .timeout(Duration::from_secs(20));
            let v = send_json(req).await?;
            let mut out = Vec::new();
            if let Some(arr) = v.get("data").and_then(|x| x.as_array()) {
                for h in arr {
                    out.push(ModSearchItem {
                        id: h.get("id").and_then(|x| x.as_i64()).unwrap_or(0).to_string(),
                        slug: h.get("slug").and_then(|x| x.as_str()).unwrap_or("").into(),
                        name: h.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                        summary: h.get("summary").and_then(|x| x.as_str()).unwrap_or("").into(),
                        downloads: h.get("downloadCount").and_then(|x| x.as_u64()).unwrap_or(0),
                        author: h
                            .get("authors")
                            .and_then(|x| x.as_array())
                            .and_then(|a| a.first())
                            .and_then(|a| a.get("name"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .into(),
                        icon: h
                            .get("logo")
                            .and_then(|x| x.get("thumbnailUrl"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .into(),
                        source: "curseforge".into(),
                    });
                }
            }
            Ok(out)
        }
        _ => Err(format!("未知来源: {source}")),
    }
}

/// 列出某模组在指定版本/加载器下的文件版本（新→旧）
pub async fn versions(
    state: &AppState,
    source: &str,
    project: &str,
    game: &str,
    loader: &str,
) -> Result<Vec<ModVersionItem>, String> {
    match source {
        "modrinth" => {
            let mut req = http(state)
                .get(&format!("https://api.modrinth.com/v2/project/{project}/version"))
                .header("user-agent", "MCS-Panel/0.1");
            if !game.is_empty() {
                req = req.query(&[("game_versions", format!(r#"["{game}"]"#))]);
            }
            if !loader.is_empty() {
                req = req.query(&[("loaders", format!(r#"["{loader}"]"#))]);
            }
            let v = send_json(req).await?;
            let mut out = Vec::new();
            if let Some(arr) = v.as_array() {
                for e in arr {
                    let files = e.get("files").and_then(|x| x.as_array());
                    let Some(file) = files
                        .and_then(|fs| {
                            fs.iter()
                                .find(|f| f.get("primary").and_then(|p| p.as_bool()).unwrap_or(false))
                        })
                        .or_else(|| files.and_then(|fs| fs.first()))
                    else {
                        continue;
                    };
                    let Some(furl) = file.get("url").and_then(|x| x.as_str()) else { continue };
                    let Some(fname) = file.get("filename").and_then(|x| x.as_str()) else { continue };
                    out.push(ModVersionItem {
                        id: e.get("id").and_then(|x| x.as_str()).unwrap_or("").into(),
                        name: e
                            .get("version_number")
                            .and_then(|x| x.as_str())
                            .or_else(|| e.get("name").and_then(|x| x.as_str()))
                            .unwrap_or("")
                            .into(),
                        filename: fname.into(),
                        url: furl.into(),
                        date: e
                            .get("date_published")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .into(),
                        dependencies: e
                            .get("dependencies")
                            .and_then(|x| x.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|d| {
                                        let pid =
                                            d.get("project_id").and_then(|x| x.as_str())?.into();
                                        let t = d
                                            .get("dependency_type")
                                            .and_then(|x| x.as_str())?
                                            .into();
                                        Some(ModDep { project_id: pid, dependency_type: t })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        sha1: file
                            .get("hashes")
                            .and_then(|h| h.get("sha1"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .into(),
                        environment: e
                            .get("environment")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .into(),
                    });
                }
            }
            Ok(out)
        }
        "curseforge" => {
            let key = cf_key(state).await?;
            let mut params: Vec<(String, String)> = Vec::new();
            if !game.is_empty() {
                params.push(("gameVersion".into(), game.into()));
            }
            let req = http(state)
                .get(&format!("https://api.curseforge.com/v1/mods/{project}/files"))
                .header("x-api-key", &key)
                .query(&params)
                .timeout(Duration::from_secs(20));
            let v = send_json(req).await?;
            let mut out = Vec::new();
            if let Some(arr) = v.get("data").and_then(|x| x.as_array()) {
                for e in arr {
                    let id = e.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
                    let filename = e.get("fileName").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let url = e
                        .get("downloadUrl")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| forgecdn_url(id, &filename));
                    out.push(ModVersionItem {
                        id: id.to_string(),
                        name: e.get("displayName").and_then(|x| x.as_str()).unwrap_or("").into(),
                        filename,
                        url,
                        date: e.get("fileDate").and_then(|x| x.as_str()).unwrap_or("").into(),
                        sha1: String::new(),
                        environment: String::new(),
                        dependencies: Vec::new(),
                    });
                }
            }
            Ok(out)
        }
        _ => Err(format!("未知来源: {source}")),
    }
}

/// CurseForge 部分文件不带 downloadUrl，用 CDN 规则构造
fn forgecdn_url(file_id: i64, filename: &str) -> String {    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    let a = file_id / 1000;
    let b = file_id % 1000;
    let encoded = utf8_percent_encode(filename, NON_ALPHANUMERIC);
    format!("https://mediafilez.forgecdn.net/files/{a}/{b}/{encoded}")
}

async fn send_json(req: reqwest::RequestBuilder) -> Result<Value, String> {
    let v = req
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("HTTP {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("解析 JSON 失败: {e}"))?;
    Ok(v)
}

/// 批量查询项目客户端/服务端支持情况（仅 Modrinth 提供该元数据）
pub async fn projects_sides(state: &AppState, ids: &[String]) -> Result<Vec<ModSideInfo>, String> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let ids_json = serde_json::to_string(&ids).map_err(|e| e.to_string())?;
    let v = http(state)
        .get("https://api.modrinth.com/v2/projects")
        .query(&[("ids", ids_json.as_str())])
        .header("user-agent", "MCS-Panel/0.1")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("HTTP {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("解析 JSON 失败: {e}"))?;
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for e in arr {
            out.push(ModSideInfo {
                id: e.get("id").and_then(|x| x.as_str()).unwrap_or("").into(),
                client_side: e
                    .get("client_side")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .into(),
                server_side: e
                    .get("server_side")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .into(),
            });
        }
    }
    Ok(out)
}

/// 通过文件 SHA1 批量查询 Modrinth 版本（用于识别已安装的模组），返回 {hash: version}
pub async fn version_files(state: &AppState, hashes: &[String]) -> Result<Value, String> {
    http(state)
        .post("https://api.modrinth.com/v2/version_files")
        .header("user-agent", "MCS-Panel/0.1")
        .json(&json!({ "hashes": hashes, "algorithm": "sha1" }))
        .timeout(Duration::from_secs(25))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("HTTP {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("解析 JSON 失败: {e}"))
}

/// CurseForge 文件指纹算法：murmur2（seed=1，按官方实现，length 视为有符号 i32）
pub fn murmur2(data: &[u8]) -> u32 {
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;
    let mut length = data.len() as i32;
    let mut h = (1i32 ^ length) as u32;
    let mut i = 0usize;
    while length >= 4 {
        let mut k = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M);
        h ^= k;
        i += 4;
        length -= 4;
    }
    match length {
        3 => {
            h ^= (data[i + 2] as u32) << 16;
            h ^= (data[i + 1] as u32) << 8;
            h ^= data[i] as u32;
            h = h.wrapping_mul(M);
        }
        2 => {
            h ^= (data[i + 1] as u32) << 8;
            h ^= data[i] as u32;
            h = h.wrapping_mul(M);
        }
        1 => {
            h ^= data[i] as u32;
            h = h.wrapping_mul(M);
        }
        _ => {}
    }
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h
}

/// 批量解析 CurseForge 文件 ID → (fileID, fileName, downloadUrl, sha1)
pub async fn cf_resolve_files(
    state: &AppState,
    file_ids: &[i64],
) -> Result<Vec<(i64, String, String, String)>, String> {
    let key = cf_key(state).await?;
    let req = http(state)
        .post("https://api.curseforge.com/v1/mods/files")
        .header("x-api-key", &key)
        .json(&json!({ "fileIds": file_ids }))
        .timeout(Duration::from_secs(40));
    let v = send_json(req).await?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(|x| x.as_array()) {
        for e in arr {
            let id = e.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
            let name = e.get("fileName").and_then(|x| x.as_str()).unwrap_or("").into();
            let url = e.get("downloadUrl").and_then(|x| x.as_str()).unwrap_or("").into();
            // hashes: [{value, algorithm}]，1 = SHA1
            let sha1 = e
                .get("hashes")
                .and_then(|x| x.as_array())
                .and_then(|hs| {
                    hs.iter()
                        .find(|h| h.get("algorithm").and_then(|a| a.as_i64()) == Some(1))
                        .and_then(|h| h.get("value").and_then(|v| v.as_str()))
                })
                .unwrap_or("")
                .to_string();
            out.push((id, name, url, sha1));
        }
    }
    Ok(out)
}

/// 通过 murmur2 指纹批量查询 CurseForge 模组（识别已安装），返回 {指纹: 项目ID}
pub async fn cf_fingerprints(state: &AppState, prints: &[u32]) -> Result<Value, String> {
    let key = cf_key(state).await?;
    let req = http(state)
        .post("https://api.curseforge.com/v1/fingerprints/432")
        .header("x-api-key", &key)
        .json(&json!({ "fingerprints": prints }))
        .timeout(Duration::from_secs(25));
    send_json(req).await
}

/// 下载模组文件到实例 mods 目录
pub async fn download_mod(
    state: &AppState,
    mods_dir: &std::path::Path,
    url: &str,
    filename: &str,
    expected_sha1: &str,
) -> Result<u64, String> {
    if !url.starts_with("https://") {
        return Err("下载地址不合法".into());
    }
    let resp = http(state)
        .get(url)
        .header("user-agent", "MCS-Panel/0.1")
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("下载失败: {e}"))?;
    let total = resp.content_length().unwrap_or(0);
    if total > 1024 * 1024 * 1024 {
        return Err("文件过大（>1GB）".into());
    }
    let bytes = resp.bytes().await.map_err(|e| format!("下载中断: {e}"))?;
    if !expected_sha1.is_empty() {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected_sha1 {
            return Err(format!("SHA1 校验失败（预期 {expected_sha1}，实际 {actual}）"));
        }
    }
    tokio::fs::write(mods_dir.join(filename), &bytes)
        .await
        .map_err(|e| format!("写入失败: {e}"))?;
    Ok(bytes.len() as u64)
}
