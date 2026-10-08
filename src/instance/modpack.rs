use crate::instance::files;
use serde_json::json;
use crate::jobs::log_job;
use crate::state::AppState;
use std::path::{Path, PathBuf};

/// 从 zip 整合包导入（CurseForge / Modrinth ServerPack / 通用服务端压缩包）
pub fn import_from_zip(
    state: &AppState,
    job_id: &str,
    instances_dir: &Path,
    zip_path: &Path,
    name: &str,
) -> Result<String, String> {
    log_job(state, job_id, format!("开始导入整合包: {}", zip_path.display()));
    let target = instances_dir.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&target).map_err(|e| format!("创建实例目录失败: {e}"))?;
    extract_zip(state, job_id, zip_path, &target)?;
    log_job(state, job_id, "应用整合包 overrides（如存在）…");
    apply_overrides(&target);
    finalize_import(state, job_id, &target, name)
}

/// 从本地已解压目录导入
pub fn import_from_dir(
    state: &AppState,
    job_id: &str,
    instances_dir: &Path,
    src: &Path,
    name: &str,
) -> Result<String, String> {
    log_job(state, job_id, format!("开始从目录导入: {}", src.display()));
    let target = instances_dir.join(uuid::Uuid::new_v4().to_string());
    copy_dir(state, job_id, src, &target)?;
    log_job(state, job_id, "应用整合包 overrides（如存在）…");
    apply_overrides(&target);
    finalize_import(state, job_id, &target, name)
}

fn extract_zip(state: &AppState, job_id: &str, zip_path: &Path, target: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("无法打开压缩包: {e}"))?;
    let mut zip =
        zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| format!("读取压缩包失败: {e}"))?;

    // 检测公共根目录（如所有条目都在 "SomePack/" 下则去掉该层）
    let mut firsts: Vec<Option<String>> = Vec::new();
    let mut all_multi = true;
    let mut file_count = 0usize;
    for i in 0..zip.len() {
        let e = zip.by_index(i).map_err(|e| format!("读取压缩包失败: {e}"))?;
        if e.is_dir() {
            continue;
        }
        let Some(rel) = e.enclosed_name() else { continue };
        file_count += 1;
        let comps: Vec<_> = rel.components().collect();
        if comps.len() < 2 {
            all_multi = false;
        }
        firsts.push(comps.first().map(|c| c.as_os_str().to_string_lossy().to_string()));
    }
    let common: Option<String> = if all_multi && file_count > 1 {
        match firsts.first().and_then(|f| f.clone()) {
            Some(f0) if firsts.iter().all(|c| c.as_deref() == Some(f0.as_str())) => Some(f0),
            _ => None,
        }
    } else {
        None
    };
    if let Some(root) = &common {
        log_job(state, job_id, format!("检测到根目录层: {root}/，已自动去除"));
    }

    let mut count = 0usize;
    for i in 0..zip.len() {
        let mut e = zip.by_index(i).map_err(|e| format!("读取压缩包失败: {e}"))?;
        let rel_owned: PathBuf = match e.enclosed_name() {
            Some(p) => p.into(),
            None => continue,
        };
        let rel: PathBuf = match &common {
            Some(root) => rel_owned.strip_prefix(root.as_str()).unwrap_or(&rel_owned).to_path_buf(),
            None => rel_owned,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = target.join(&rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|err| format!("{err}"))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("{err}"))?;
        }
        let mut out = std::fs::File::create(&out_path).map_err(|err| format!("写入 {} 失败: {err}", rel.display()))?;
        std::io::copy(&mut e, &mut out).map_err(|err| format!("解压 {} 失败: {err}", rel.display()))?;
        count += 1;
        if count % 200 == 0 {
            log_job(state, job_id, format!("已解压 {count} 个文件…"));
        }
    }
    log_job(state, job_id, format!("解压完成，共 {count} 个文件"));
    Ok(())
}

fn copy_dir(state: &AppState, job_id: &str, src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("{e}"))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("读取目录失败: {e}"))?;
    let mut n = 0usize;
    for e in entries.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir(state, job_id, &from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|err| format!("复制 {} 失败: {err}", from.display()))?;
            n += 1;
            if n % 200 == 0 {
                log_job(state, job_id, format!("已复制 {n} 个文件…"));
            }
        }
    }
    Ok(())
}

/// CurseForge/Modrinth 客户端包结构：manifest.json + overrides/
fn apply_overrides(target: &Path) {
    let has_pack = target.join("manifest.json").exists() || target.join("modrinth.index.json").exists();
    let overrides = target.join("overrides");
    if has_pack && overrides.is_dir() {
        move_dir_contents(&overrides, target);
        let _ = std::fs::remove_dir(&overrides);
    }
}

fn move_dir_contents(src: &Path, dst: &Path) {
    let Ok(entries) = std::fs::read_dir(src) else { return };
    for e in entries.flatten() {
        let to = dst.join(e.file_name());
        if std::fs::rename(e.path(), &to).is_err() {
            if e.path().is_dir() {
                let _ = std::fs::create_dir_all(&to);
                move_dir_contents(&e.path(), &to);
                let _ = std::fs::remove_dir_all(e.path());
            } else {
                let _ = std::fs::copy(e.path(), &to);
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn finalize_import(state: &AppState, job_id: &str, target: &Path, name: &str) -> Result<String, String> {
    let jars = files::list_jars(target);
    let (jar, jvm_args) = detect_launch(target, &jars);

    if let Some(j) = &jar {
        log_job(state, job_id, format!("识别到主程序: {j}"));
    }
    if let Some(a) = &jvm_args {
        log_job(state, job_id, format!("识别到启动参数: {a}"));
    }
    if jar.is_none() && jvm_args.is_none() {
        if jars.is_empty() {
            log_job(state, job_id, "⚠ 未找到服务器 JAR，请在实例设置中配置主程序");
        } else {
            log_job(
                state,
                job_id,
                format!("⚠ 未自动识别主程序，请在实例设置中选择（发现 {} 个 JAR）", jars.len()),
            );
        }
    }
    // 检查 @argfile 是否存在（Forge/NeoForge 1.17+ 需要 libraries 已安装）
    if let Some(args) = &jvm_args {
        for a in args.split_whitespace() {
            if let Some(p) = a.strip_prefix('@') {
                if !target.join(p).exists() {
                    log_job(
                        state,
                        job_id,
                        format!("⚠ 启动参数引用的文件不存在: {p}（可能需要先运行安装器）"),
                    );
                }
            }
        }
    }

    let meta = crate::instance::InstanceMeta {
        id: target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        name: name.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        java_path: None,
        min_ram_mb: 1024,
        max_ram_mb: 4096,
        jar,
        jvm_args: jvm_args.unwrap_or_default(),
        auto_restart: false,
        auto_start_on_boot: false,
        mc_version: None,
        mod_loader: None,
        ..Default::default()
    };
    std::fs::write(
        target.join("instance.json"),
        serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写入实例信息失败: {e}"))?;

    let rt = crate::instance::InstanceRuntime::new(meta.clone(), target.to_path_buf());
    state.instances.blocking_write().insert(meta.id.clone(), rt);
    log_job(state, job_id, format!("✅ 导入完成！实例「{}」已创建", meta.name));
    Ok(meta.id)
}

/// 从启动脚本与目录中识别主程序与启动参数
pub fn detect_launch(dir: &Path, jars: &[String]) -> (Option<String>, Option<String>) {
    for script in ["run.bat", "start.bat", "run.sh", "start.sh"] {
        let p = dir.join(script);
        let Ok(s) = std::fs::read_to_string(&p) else { continue };

        // Forge/NeoForge 1.17+：java @user_jvm_args.txt @libraries/.../win_args.txt
        let at_args: Vec<String> = s
            .split_whitespace()
            .map(|t| t.trim_matches('"').to_string())
            .filter(|t| t.starts_with('@') && t.ends_with("args.txt"))
            .collect();
        if !at_args.is_empty() {
            return (None, Some(at_args.join(" ")));
        }

        // 老式启动脚本：java -jar xxx.jar nogui
        let re = regex::Regex::new(r#"-jar\s+"?([^"\s]+\.jar)"?"#).unwrap();
        if let Some(c) = re.captures(&s) {
            if let Some(m) = c.get(1) {
                let jar = m.as_str();
                if dir.join(jar).exists() {
                    return (Some(jar.to_string()), None);
                }
            }
        }
    }

    // 扫描目录中的 jar，按名称特征打分
    let mut best: Option<(i32, String)> = None;
    for j in jars {
        let low = j.to_lowercase();
        let name_low = low.rsplit('/').next().unwrap_or("");
        if name_low.contains("installer") || name_low.contains("sources") || name_low.contains("javadoc") {
            continue;
        }
        let mut score = 0;
        if name_low.contains("server") {
            score += 5;
        }
        if name_low.contains("neoforge") {
            score += 3;
        }
        if name_low.contains("forge") {
            score += 3;
        }
        if name_low.contains("fabric") {
            score += 2;
        }
        if name_low.contains("quilt") {
            score += 2;
        }
        if !low.contains('/') {
            score += 1;
        }
        if score > 0 && best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, j.clone()));
        }
    }
    if best.is_none() && jars.len() == 1 {
        return (Some(jars[0].clone()), None);
    }
    best.map(|(_, j)| (Some(j), None)).unwrap_or((None, None))
}


// ================= 整合包更新 =================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackMod {
    pub filename: String,
    #[serde(default)]
    pub url: String, // 空 = 文件已在临时目录 mods/ 下（通用服务端包），直接复制
    #[serde(default)]
    pub sha1: String,
}

/// 预览结果缓存（存入临时目录，apply 时读取，避免二次解析与二次调用 CF API）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackCache {
    pub pack_type: String, // curseforge | modrinth | generic
    pub name: String,
    pub mc_version: String,
    pub loader: Option<String>,
    pub loader_version: Option<String>,
    pub mods: Vec<PackMod>,
    pub cf_unresolved: usize,
    pub overrides_files: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PackPreview {
    pub preview_id: String,
    pub pack_type: String,
    pub name: String,
    pub mc_version: String,
    pub loader: Option<String>,
    pub loader_version: Option<String>,
    pub mods: Vec<PackMod>,
    pub cf_unresolved: usize,
    pub overrides_files: u64,
}

enum RawPack {
    CurseForge {
        name: String,
        mc: String,
        loader: Option<String>,
        lver: Option<String>,
        files: Vec<(i64, i64)>, // (projectID, fileID)
    },
    Modrinth {
        name: String,
        mc: String,
        loader: Option<String>,
        lver: Option<String>,
        mods: Vec<PackMod>,
    },
    Generic,
}

/// 解析已解压目录中的整合包描述文件，同时统计 overrides 文件数
fn scan_extracted(dir: &Path) -> Result<(RawPack, u64), String> {
    let overrides = dir.join("overrides");
    let is_pack = dir.join("manifest.json").is_file() || dir.join("modrinth.index.json").is_file();
    let overrides_files = if is_pack {
        if overrides.is_dir() {
            count_files(&overrides)
        } else {
            0
        }
    } else {
        // 通用服务端包：mods/ 之外的文件都算"配置与资源"
        count_files_excl(dir, &["mods", "manifest.json", "modrinth.index.json"])
    };

    if let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) {
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("manifest.json 解析失败: {e}"))?;
        let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("CurseForge 整合包").into();
        let mc = v
            .pointer("/minecraft/version")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let mut loader = None;
        let mut lver = None;
        if let Some(mls) = v.pointer("/minecraft/modLoaders").and_then(|x| x.as_array()) {
            if let Some(m) = mls.first() {
                // id 形如 "forge-47.2.0" / "fabric-0.15.11"
                if let Some((l, lv)) = m.get("id").and_then(|x| x.as_str()).unwrap_or("").split_once('-') {
                    loader = Some(l.to_string());
                    lver = Some(lv.to_string());
                }
            }
        }
        let mut files = Vec::new();
        if let Some(arr) = v.get("files").and_then(|x| x.as_array()) {
            for f in arr {
                let pid = f.get("projectID").and_then(|x| x.as_i64()).unwrap_or(0);
                let fid = f.get("fileID").and_then(|x| x.as_i64()).unwrap_or(0);
                if fid > 0 {
                    files.push((pid, fid));
                }
            }
        }
        return Ok((RawPack::CurseForge { name, mc, loader, lver, files }, overrides_files));
    }

    if let Ok(text) = std::fs::read_to_string(dir.join("modrinth.index.json")) {
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("modrinth.index.json 解析失败: {e}"))?;
        let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("Modrinth 整合包").into();
        let mc = v.get("gameVersion").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let deps = v.get("dependencies").cloned().unwrap_or(json!({}));
        let (loader, lver) = ["forge", "neoforge", "fabric", "quilt"]
            .iter()
            .find_map(|k| {
                deps.get(*k)
                    .and_then(|x| x.as_str())
                    .map(|lv| (Some(k.to_string()), Some(lv.to_string())))
            })
            .unwrap_or((None, None));
        let mut mods = Vec::new();
        if let Some(arr) = v.get("files").and_then(|x| x.as_array()) {
            for f in arr {
                let path = f.get("path").and_then(|x| x.as_str()).unwrap_or("");
                let url = f
                    .pointer("/downloads/0")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let sha1 = f
                    .pointer("/hashes/sha1")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if path.is_empty() || url.is_empty() {
                    continue;
                }
                let filename = path.replace('\\', "/");
                let filename = filename.rsplit('/').next().unwrap_or(path).to_string();
                mods.push(PackMod { filename, url, sha1 });
            }
        }
        return Ok((RawPack::Modrinth { name, mc, loader, lver, mods }, overrides_files));
    }

    Ok((RawPack::Generic, overrides_files))
}

fn count_files(dir: &Path) -> u64 {
    let mut n = 0u64;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += count_files(&p);
        } else {
            n += 1;
        }
    }
    n
}

fn count_files_excl(dir: &Path, excl: &[&str]) -> u64 {
    let mut n = 0u64;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if excl.iter().any(|x| name == *x) {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            n += count_files(&p);
        } else {
            n += 1;
        }
    }
    n
}

/// 通用服务端包：从解压目录的 mods/ 收集 jar（url 留空表示从临时目录复制）
fn collect_generic_mods(dir: &Path) -> Vec<PackMod> {
    let mods_dir = dir.join("mods");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&mods_dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let low = name.to_lowercase();
        if !p.is_file() || !low.ends_with(".jar") {
            continue;
        }
        let sha1 = std::fs::File::open(&p)
            .ok()
            .and_then(|mut f| {
                use sha1::{Digest, Sha1};
                let mut h = Sha1::new();
                std::io::copy(&mut f, &mut h).ok()?;
                Some(format!("{:x}", h.finalize()))
            })
            .unwrap_or_default();
        out.push(PackMod { filename: name, url: String::new(), sha1 });
    }
    out
}

/// 生成预览：解压 zip 到临时目录并解析；CurseForge 包在此步解析文件 ID
pub async fn preview_modpack(
    state: &AppState,
    zip_path: &Path,
    preview_id: &str,
) -> Result<PackPreview, String> {
    let temp_dir = std::env::temp_dir().join(format!("mcspr_pack_{preview_id}"));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("创建临时目录失败: {e}"))?;

    let zp = zip_path.to_path_buf();
    let td = temp_dir.clone();
    tokio::task::spawn_blocking(move || extract_zip_dir(&zp, &td))
        .await
        .map_err(|e| format!("解压任务失败: {e}"))??;
    let (raw, overrides_files) = scan_extracted(&temp_dir)?;

    let cache = match raw {
        RawPack::CurseForge { name, mc, loader, lver, files } => {
            let ids: Vec<i64> = files.iter().map(|f| f.1).collect();
            let resolved = crate::instance::moddb::cf_resolve_files(state, &ids).await?;
            let map: std::collections::HashMap<i64, (String, String, String)> = resolved
                .into_iter()
                .map(|(id, n, u, s)| (id, (n, u, s)))
                .collect();
            let mut mods = Vec::new();
            let mut unresolved = 0usize;
            for (_pid, fid) in &files {
                match map.get(fid) {
                    Some((name, url, sha1)) => {
                        let url = if url.is_empty() { forgecdn_url(*fid, name) } else { url.clone() };
                        mods.push(PackMod { filename: name.clone(), url, sha1: sha1.clone() });
                    }
                    None => unresolved += 1,
                }
            }
            PackCache {
                pack_type: "curseforge".into(),
                name,
                mc_version: mc,
                loader,
                loader_version: lver,
                mods,
                cf_unresolved: unresolved,
                overrides_files,
            }
        }
        RawPack::Modrinth { name, mc, loader, lver, mods } => PackCache {
            pack_type: "modrinth".into(),
            name,
            mc_version: mc,
            loader,
            loader_version: lver,
            mods,
            cf_unresolved: 0,
            overrides_files,
        },
        RawPack::Generic => {
            let mods = collect_generic_mods(&temp_dir);
            let name = zip_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "服务端包".into());
            PackCache {
                pack_type: "generic".into(),
                name,
                mc_version: String::new(),
                loader: None,
                loader_version: None,
                mods,
                cf_unresolved: 0,
                overrides_files,
            }
        }
    };

    std::fs::write(
        temp_dir.join("pack_cache.json"),
        serde_json::to_string(&cache).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写入缓存失败: {e}"))?;

    Ok(PackPreview {
        preview_id: preview_id.to_string(),
        pack_type: cache.pack_type.clone(),
        name: cache.name.clone(),
        mc_version: cache.mc_version.clone(),
        loader: cache.loader.clone(),
        loader_version: cache.loader_version.clone(),
        mods: cache.mods.clone(),
        cf_unresolved: cache.cf_unresolved,
        overrides_files: cache.overrides_files,
    })
}

/// forgecdn 直链构造（CF API 未返回 downloadUrl 时的兜底）
fn forgecdn_url(file_id: i64, filename: &str) -> String {
    format!(
        "https://mediafilez.forgecdn.net/files/{}/{}/{}",
        file_id / 1000,
        file_id % 1000,
        filename
    )
}

fn extract_zip_dir(zip_path: &Path, target: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("无法打开压缩包: {e}"))?;
    let mut zip =
        zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| format!("读取压缩包失败: {e}"))?;
    for i in 0..zip.len() {
        let mut e = zip.by_index(i).map_err(|e| format!("读取压缩包失败: {e}"))?;
        let rel: PathBuf = match e.enclosed_name() {
            Some(p) => p.into(),
            None => continue,
        };
        let out_path = target.join(&rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|err| format!("{err}"))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("{err}"))?;
        }
        let mut out =
            std::fs::File::create(&out_path).map_err(|err| format!("写入 {} 失败: {err}", rel.display()))?;
        std::io::copy(&mut e, &mut out).map_err(|err| format!("解压 {} 失败: {err}", rel.display()))?;
    }
    Ok(())
}

/// 更新时绝不覆盖的顶层条目（世界 / 服务器设置 / 实例信息）
const PROTECTED: &[&str] = &[
    "world", "world_nether", "world_the_end", "servers.dat", "server.properties",
    "eula.txt", "instance.json", "logs", "session.lock",
];

/// 应用 overrides：包内文件覆盖实例同名文件；包里没有的旧文件保留
fn copy_overrides_update(src: &Path, dst: &Path, top_level: bool) -> Result<u64, String> {
    let Ok(entries) = std::fs::read_dir(src) else { return Ok(0) };
    let mut n = 0u64;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let low = name.to_lowercase();
        if top_level && PROTECTED.iter().any(|p| low == *p) {
            continue;
        }
        let to = dst.join(&name);
        let from = e.path();
        if from.is_dir() {
            std::fs::create_dir_all(&to).map_err(|err| format!("{err}"))?;
            n += copy_overrides_update(&from, &to, false)?;
        } else {
            std::fs::copy(&from, &to).map_err(|err| format!("覆盖 {} 失败: {err}", name))?;
            n += 1;
        }
    }
    Ok(n)
}

/// 应用整合包更新（在后台任务中执行）
pub async fn apply_modpack_update(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    preview_id: &str,
    backup_first: bool,
    orphan_mode: &str,   // keep | disable | delete
    allow_reinstall: bool,
) -> Result<(), String> {
    if !preview_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("预览 ID 不合法".into());
    }
    let temp_dir = std::env::temp_dir().join(format!("mcspr_pack_{preview_id}"));
    let cache_path = temp_dir.join("pack_cache.json");
    if !cache_path.is_file() {
        return Err("预览已过期，请重新选择整合包并解析".into());
    }
    let cache: PackCache = serde_json::from_str(
        &std::fs::read_to_string(&cache_path).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("解析预览缓存失败: {e}"))?;

    let rt = crate::instance::get_instance(state, instance_id)
        .await
        .map_err(|e| e.to_string())?;
    if *rt.status.lock().await != crate::instance::Status::Stopped {
        return Err("更新前请先停止实例".into());
    }

    log_job(
        state,
        job_id,
        format!("开始更新整合包：{}（{}）", cache.name, cache.pack_type),
    );

    // 1. 更新前备份
    if backup_first {
        log_job(state, job_id, "正在创建更新前备份…");
        let name = crate::instance::backup::create(state, &rt).await?;
        log_job(state, job_id, format!("✅ 备份完成：{name}（可在「备份」页恢复）"));
    }

    // 2. MC 版本 / 加载器变化 → 重装加载器
    let (cur_mc, cur_loader) = {
        let m = rt.meta.read().await;
        (m.mc_version.clone().unwrap_or_default(), m.mod_loader.clone().unwrap_or_default())
    };
    let need_reinstall = allow_reinstall
        && !cache.mc_version.is_empty()
        && (cache.mc_version != cur_mc || cache.loader.clone().unwrap_or_default() != cur_loader);
    if !cache.mc_version.is_empty() && cache.mc_version != cur_mc && !allow_reinstall {
        log_job(
            state,
            job_id,
            format!("⚠ 整合包要求 MC {0}，当前 {1}，且未开启自动重装，请手动重装后再次更新", cache.mc_version, cur_mc),
        );
    }
    if need_reinstall {
        let loader = cache.loader.clone().unwrap_or_else(|| "vanilla".into());
        let lver = cache.loader_version.clone().unwrap_or_default();
        if loader != "vanilla" && lver.is_empty() {
            return Err(format!("整合包未声明 {loader} 版本号，无法自动重装"));
        }
        log_job(
            state,
            job_id,
            format!(
                "检测到版本变化（{} → {}），开始重装加载器…",
                if cur_mc.is_empty() { "未记录" } else { &cur_mc },
                cache.mc_version
            ),
        );
        crate::instance::loaders::install_inner(state, job_id, instance_id, &loader, &cache.mc_version, &lver)
            .await?;
    } else if !cache.mc_version.is_empty() {
        log_job(state, job_id, format!("MC 版本/加载器无变化（{cur_mc}），跳过重装"));
    }

    // 3. 应用 overrides（config / kubejs / scripts 等，包内文件覆盖）
    let overrides_src = if cache.pack_type == "generic" { temp_dir.clone() } else { temp_dir.join("overrides") };
    if overrides_src.is_dir() {
        log_job(state, job_id, "应用整合包配置与资源文件…");
        let n = copy_overrides_update(&overrides_src, &rt.dir, true)?;
        log_job(state, job_id, format!("✅ 已覆盖 {n} 个文件（world / server.properties / 实例设置不受影响）"));
    }

    // 4. 同步 mods
    if !cache.mods.is_empty() {
        let mods_dir = crate::instance::mods::mods_dir(&rt).await;
        std::fs::create_dir_all(&mods_dir).map_err(|e| format!("{e}"))?;
        log_job(state, job_id, format!("同步模组：包内共 {} 个", cache.mods.len()));
        for (i, m) in cache.mods.iter().enumerate() {
            let target = mods_dir.join(&m.filename);
            if target.is_file() && !m.sha1.is_empty() {
                let same = std::fs::File::open(&target).ok().and_then(|mut f| {
                    use sha1::{Digest, Sha1};
                    let mut h = Sha1::new();
                    std::io::copy(&mut f, &mut h).ok()?;
                    Some(format!("{:x}", h.finalize()) == m.sha1)
                });
                if same == Some(true) {
                    continue;
                }
            }
            if m.url.is_empty() {
                let src = temp_dir.join("mods").join(&m.filename);
                if src.is_file() {
                    std::fs::copy(&src, &target).map_err(|e| format!("复制 {} 失败: {e}", m.filename))?;
                } else {
                    log_job(state, job_id, format!("⚠ 包内缺失文件：{}", m.filename));
                    continue;
                }
            } else {
                log_job(
                    state,
                    job_id,
                    format!("下载模组 ({}/{})：{}", i + 1, cache.mods.len(), m.filename),
                );
                crate::instance::moddb::download_mod(state, &mods_dir, &m.url, &m.filename, &m.sha1).await?;
            }
        }
        log_job(state, job_id, "✅ 模组同步完成");

        // 5. 处理包外模组
        let pack_names: std::collections::HashSet<String> =
            cache.mods.iter().map(|m| m.filename.to_lowercase()).collect();
        let mut orphans: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&mods_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let low = name.to_lowercase();
                if !low.ends_with(".jar") && !low.ends_with(".jar.disabled") {
                    continue;
                }
                let base = low.strip_suffix(".disabled").unwrap_or(&low);
                if !pack_names.contains(base) {
                    orphans.push(e.path());
                }
            }
        }
        match orphan_mode {
            "delete" => {
                for p in &orphans {
                    let _ = std::fs::remove_file(p);
                }
                if !orphans.is_empty() {
                    log_job(state, job_id, format!("✅ 已删除 {} 个整合包未包含的模组", orphans.len()));
                }
            }
            "disable" => {
                let mut n = 0usize;
                for p in &orphans {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                    if name.to_lowercase().ends_with(".jar") {
                        let to = p.with_file_name(format!("{name}.disabled"));
                        if std::fs::rename(p, &to).is_ok() {
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    log_job(
                        state,
                        job_id,
                        format!("✅ 已禁用 {} 个整合包未包含的模组（改名 .disabled，可随时恢复）", n),
                    );
                }
            }
            _ => {
                if !orphans.is_empty() {
                    log_job(state, job_id, format!("保留 {} 个整合包未包含的模组（未做处理）", orphans.len()));
                }
            }
        }
    }

    // 6. 更新实例元数据（persist 内部会请求 meta 读锁，必须在写锁释放后调用，否则死锁）
    {
        let mut m = rt.meta.write().await;
        if !cache.mc_version.is_empty() {
            m.mc_version = Some(cache.mc_version.clone());
        }
        if let Some(l) = &cache.loader {
            m.mod_loader = Some(l.clone());
        }
    }
    if let Err(e) = rt.persist().await {
        log_job(state, job_id, format!("⚠ 实例信息写入失败: {e}"));
    }

    // 7. 清理临时目录
    let _ = std::fs::remove_dir_all(&temp_dir);

    log_job(state, job_id, "✅ 整合包更新完成！请启动服务器验证");
    Ok(())
}
