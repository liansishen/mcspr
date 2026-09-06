use crate::state::AppState;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub status: String, // running | done | error
    pub progress: u8,   // 0-100，仅下载类任务使用
    pub logs: Vec<String>,
    pub instance_id: Option<String>,
}

impl Job {
    pub fn new(id: String) -> Self {
        Self {
            id,
            status: "running".into(),
            progress: 0,
            logs: Vec::new(),
            instance_id: None,
        }
    }

    pub fn log(&mut self, msg: impl Into<String>) {
        let line = format!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg.into());
        self.logs.push(line);
        if self.logs.len() > 400 {
            self.logs.drain(0..self.logs.len() - 400);
        }
    }
}

pub fn log_job(state: &AppState, id: &str, msg: impl Into<String>) {
    if let Some(j) = state.jobs.lock().unwrap_or_else(|p| p.into_inner()).get_mut(id) {
        j.log(msg);
    }
}

pub fn set_progress(state: &AppState, id: &str, pct: u8) {
    if let Some(j) = state.jobs.lock().unwrap_or_else(|p| p.into_inner()).get_mut(id) {
        j.progress = pct;
    }
}

pub fn finish_job(state: &AppState, id: &str, err: Option<String>, instance_id: Option<String>) {
    if let Some(j) = state.jobs.lock().unwrap_or_else(|p| p.into_inner()).get_mut(id) {
        match err {
            Some(e) => {
                j.status = "error".into();
                j.log(format!("❌ {e}"));
            }
            None => j.status = "done".into(),
        }
        j.instance_id = instance_id;
    }
}
