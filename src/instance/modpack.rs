use crate::instance::files;
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
