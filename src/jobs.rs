use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub id: Uuid,
    pub filename: String,
    pub input_path: PathBuf,
    pub result_path: PathBuf,
    pub archive_path: Option<PathBuf>,
    pub status: JobStatus,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default)]
pub struct JobStore {
    inner: Arc<Mutex<JobStoreInner>>,
}

#[derive(Debug, Default)]
struct JobStoreInner {
    jobs: HashMap<Uuid, Job>,
    active_by_path: HashMap<PathBuf, Uuid>,
    recent: VecDeque<Uuid>,
}

impl JobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_or_get(&self, input_path: PathBuf, result_path: PathBuf) -> (Job, bool) {
        let mut inner = self.inner.lock().expect("job store lock poisoned");
        if let Some(id) = inner.active_by_path.get(&input_path)
            && let Some(job) = inner.jobs.get(id)
        {
            return (job.clone(), false);
        }

        let now = Utc::now();
        let id = Uuid::new_v4();
        let filename = input_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| input_path.display().to_string());
        let job = Job {
            id,
            filename,
            input_path: input_path.clone(),
            result_path,
            archive_path: None,
            status: JobStatus::Queued,
            error: None,
            created_at: now,
            updated_at: now,
        };
        inner.active_by_path.insert(input_path, id);
        inner.recent.push_front(id);
        inner.jobs.insert(id, job.clone());
        (job, true)
    }

    pub fn list_recent(&self, limit: usize) -> Vec<Job> {
        let inner = self.inner.lock().expect("job store lock poisoned");
        inner
            .recent
            .iter()
            .take(limit)
            .filter_map(|id| inner.jobs.get(id).cloned())
            .collect()
    }

    pub fn get(&self, id: Uuid) -> Option<Job> {
        self.inner
            .lock()
            .expect("job store lock poisoned")
            .jobs
            .get(&id)
            .cloned()
    }

    pub fn mark_running(&self, id: Uuid) {
        self.update(id, |job| {
            job.status = JobStatus::Running;
            job.error = None;
        });
    }

    pub fn mark_failed(&self, id: Uuid, error: impl Into<String>) {
        self.finish(id, |job| {
            job.status = JobStatus::Failed;
            job.error = Some(error.into());
        });
    }

    pub fn mark_succeeded(&self, id: Uuid, archive_path: PathBuf) {
        self.finish(id, |job| {
            job.status = JobStatus::Succeeded;
            job.archive_path = Some(archive_path);
            job.error = None;
        });
    }

    fn update(&self, id: Uuid, f: impl FnOnce(&mut Job)) {
        let mut inner = self.inner.lock().expect("job store lock poisoned");
        if let Some(job) = inner.jobs.get_mut(&id) {
            f(job);
            job.updated_at = Utc::now();
        }
    }

    fn finish(&self, id: Uuid, f: impl FnOnce(&mut Job)) {
        let mut inner = self.inner.lock().expect("job store lock poisoned");
        let input_path = if let Some(job) = inner.jobs.get_mut(&id) {
            f(job);
            job.updated_at = Utc::now();
            Some(job.input_path.clone())
        } else {
            None
        };
        if let Some(input_path) = input_path {
            inner.active_by_path.remove(&input_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{JobStatus, JobStore};
    use std::path::PathBuf;

    #[test]
    fn active_jobs_are_deduplicated_by_input_path() {
        let store = JobStore::new();
        let input = PathBuf::from("/data/input/a.txt");
        let result = PathBuf::from("/data/results/a.txt.md");
        let (first, queued_first) = store.create_or_get(input.clone(), result.clone());
        let (second, queued_second) = store.create_or_get(input.clone(), result);

        assert!(queued_first);
        assert!(!queued_second);
        assert_eq!(first.id, second.id);

        store.mark_failed(first.id, "boom");
        let (third, queued_third) =
            store.create_or_get(input, PathBuf::from("/data/results/a.txt.md"));
        assert!(queued_third);
        assert_ne!(first.id, third.id);
        assert_eq!(store.get(first.id).unwrap().status, JobStatus::Failed);
    }
}
