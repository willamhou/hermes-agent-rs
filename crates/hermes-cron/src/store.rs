use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::job::CronJob;

#[derive(Serialize, Deserialize)]
struct JobsFile {
    jobs: Vec<CronJob>,
    updated_at: String,
}

pub struct JobStore {
    path: PathBuf,
}

impl JobStore {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Use create_new so two concurrent openers don't both write the initial
        // file — the losing process gets AlreadyExists, which is fine.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let empty = JobsFile {
                    jobs: vec![],
                    updated_at: chrono::Utc::now().to_rfc3339(),
                };
                let json = serde_json::to_string_pretty(&empty)?;
                f.write_all(json.as_bytes())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process created it concurrently — that's fine.
            }
            Err(e) => return Err(e.into()),
        }
        Ok(Self { path })
    }

    /// Expose the backing file path (used by the scheduler for lock file placement).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn list(&self) -> anyhow::Result<Vec<CronJob>> {
        let content = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {:?}", self.path))?;
        let file: JobsFile = serde_json::from_str(&content)?;
        Ok(file.jobs)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<CronJob>> {
        Ok(self.list()?.into_iter().find(|j| j.id == id))
    }

    pub fn create(&self, job: CronJob) -> anyhow::Result<()> {
        let mut jobs = self.list()?;
        jobs.push(job);
        self.write(jobs)
    }

    pub fn update(&self, job: CronJob) -> anyhow::Result<bool> {
        let mut jobs = self.list()?;
        let found = if let Some(existing) = jobs.iter_mut().find(|j| j.id == job.id) {
            *existing = job;
            true
        } else {
            false
        };
        if found {
            self.write(jobs)?;
        }
        Ok(found)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<bool> {
        let mut jobs = self.list()?;
        let len_before = jobs.len();
        jobs.retain(|j| j.id != id);
        let removed = jobs.len() < len_before;
        self.write(jobs)?;
        Ok(removed)
    }

    fn write(&self, jobs: Vec<CronJob>) -> anyhow::Result<()> {
        let file = JobsFile {
            jobs,
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;

        // Keep one best-effort backup before replacing the live file.
        let bak = self.path.with_extension("json.bak");
        if self.path.exists() {
            let _ = std::fs::copy(&self.path, &bak);
        }

        // Atomic rename: tmp → live.
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{CronJob, JobSchedule};
    use tempfile::TempDir;

    fn make_store(dir: &TempDir) -> JobStore {
        JobStore::open(dir.path().join("jobs.json")).unwrap()
    }

    fn make_job(name: &str) -> CronJob {
        CronJob::new(
            name.to_string(),
            "test prompt".to_string(),
            JobSchedule::Interval { minutes: 60 },
            "stdout".to_string(),
        )
    }

    #[test]
    fn empty_store_list_returns_empty() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let jobs = store.list().unwrap();
        assert!(jobs.is_empty());
    }

    #[test]
    fn create_then_get_found() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let job = make_job("alpha");
        let id = job.id.clone();
        store.create(job).unwrap();
        let found = store.get(&id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, id);
    }

    #[test]
    fn create_then_list_contains_job() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let job = make_job("beta");
        let id = job.id.clone();
        store.create(job).unwrap();
        let list = store.list().unwrap();
        assert!(list.iter().any(|j| j.id == id));
    }

    #[test]
    fn update_changes_fields() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let mut job = make_job("gamma");
        store.create(job.clone()).unwrap();
        job.name = "gamma-updated".to_string();
        assert!(store.update(job.clone()).unwrap());
        let found = store.get(&job.id).unwrap().unwrap();
        assert_eq!(found.name, "gamma-updated");
    }

    #[test]
    fn remove_job_is_gone() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let job = make_job("delta");
        let id = job.id.clone();
        store.create(job).unwrap();
        let removed = store.remove(&id).unwrap();
        assert!(removed);
        assert!(store.get(&id).unwrap().is_none());
    }

    #[test]
    fn create_multiple_list_returns_all() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let j1 = make_job("j1");
        let j2 = make_job("j2");
        let j3 = make_job("j3");
        let ids: Vec<_> = [j1.id.clone(), j2.id.clone(), j3.id.clone()].into();
        store.create(j1).unwrap();
        store.create(j2).unwrap();
        store.create(j3).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 3);
        for id in &ids {
            assert!(list.iter().any(|j| &j.id == id));
        }
    }
}
