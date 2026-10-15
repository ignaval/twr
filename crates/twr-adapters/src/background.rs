//! Session-scoped task tracking shared by structured adapters. Terminal signals win over
//! late progress; duplicate inventory observations only renew the lease once a minute.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use twr_core::event::TaskUpdated;

#[derive(Debug, Default)]
pub(crate) struct Tasks {
    pub(crate) running: HashMap<String, (String, Instant)>,
    progress_at: HashMap<String, Instant>,
    finished: HashSet<String>,
    order: VecDeque<String>,
}

impl Tasks {
    pub(crate) fn knows(&self, id: &str) -> bool {
        self.running.contains_key(id) || self.finished.contains(id)
    }

    pub(crate) fn reopen(&mut self, id: &str) {
        self.finished.remove(id);
        self.order.retain(|old| old != id);
        self.progress_at.remove(id);
    }
    pub(crate) fn update(
        &mut self,
        id: &str,
        running: bool,
        description: &str,
    ) -> Option<TaskUpdated> {
        if id.is_empty() || id.len() > 512 || (running && self.finished.contains(id)) {
            return None;
        }
        let mut heartbeat = false;
        let description = if running {
            if let Some((old, seen)) = self.running.get(id) {
                if description.is_empty() || old == description {
                    if seen.elapsed() < Duration::from_secs(60) {
                        return None;
                    }
                    heartbeat = true;
                }
                if description.is_empty() {
                    old.clone()
                } else {
                    description.to_string()
                }
            } else {
                description.to_string()
            }
        } else {
            self.progress_at.remove(id);
            if !self.finished.insert(id.to_string()) {
                return None;
            }
            self.order.push_back(id.to_string());
            if self.order.len() > 4096 {
                if let Some(old) = self.order.pop_front() {
                    self.finished.remove(&old);
                }
            }
            self.running
                .remove(id)
                .map_or_else(|| description.to_string(), |(d, _)| d)
        };
        if running {
            self.running
                .insert(id.to_string(), (description.clone(), Instant::now()));
        }
        Some(TaskUpdated {
            task_id: id.to_string(),
            running,
            description,
            heartbeat,
            ..Default::default()
        })
    }

    /// Progress is real activity even when the description has not changed.
    pub(crate) fn progress(&mut self, id: &str, description: &str) -> Option<TaskUpdated> {
        if self
            .running
            .get(id)
            .is_some_and(|(old, _)| description.is_empty() || old == description)
            && self
                .progress_at
                .get(id)
                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return None;
        }
        let mut event = self.update(id, true, description).or_else(|| {
            let (description, seen) = self.running.get_mut(id)?;
            *seen = Instant::now();
            Some(TaskUpdated {
                task_id: id.to_string(),
                running: true,
                description: description.clone(),
                ..Default::default()
            })
        })?;
        self.progress_at.insert(id.to_string(), Instant::now());
        event.heartbeat = false;
        Some(event)
    }
}

pub(crate) fn terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "stopped" | "killed" | "cancelled"
    )
}
