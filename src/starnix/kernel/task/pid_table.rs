// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::memory_attribution::MemoryAttributionLifecycleEvent;
use crate::task::{ProcessGroup, Task, ThreadGroup, ZombieProcess};
use starnix_logging::track_stub;
use starnix_types::ownership::{TempRef, WeakRef};
use starnix_uapi::{pid_t, tid_t};
use std::collections::HashMap;
use std::sync::{Arc, Weak};

// The maximal pid considered.
const PID_MAX_LIMIT: pid_t = 1 << 15;

#[derive(Default, Debug)]
enum ProcessEntry {
    #[default]
    None,
    ThreadGroup(WeakRef<ThreadGroup>),
    Zombie(WeakRef<ZombieProcess>),
}

impl ProcessEntry {
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    fn thread_group(&self) -> Option<&WeakRef<ThreadGroup>> {
        match self {
            Self::ThreadGroup(ref group) => Some(group),
            _ => None,
        }
    }
}

/// Entities identified by a pid.
#[derive(Default, Debug)]
struct PidEntry {
    task: Option<WeakRef<Task>>,
    process: ProcessEntry,
    process_group: Option<Weak<ProcessGroup>>,
}

impl PidEntry {
    fn is_empty(&self) -> bool {
        self.task.is_none() && self.process.is_none() && self.process_group.is_none()
    }
}

pub enum ProcessEntryRef<'a> {
    Process(TempRef<'a, ThreadGroup>),
    Zombie(TempRef<'a, ZombieProcess>),
}

#[derive(Default, Debug)]
pub struct PidTable {
    /// The most-recently allocated pid in this table.
    last_pid: pid_t,

    /// The tasks in this table, organized by pid_t.
    table: HashMap<pid_t, PidEntry>,

    /// Used to notify thread group changes.
    thread_group_notifier: Option<std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>>,
}

impl PidTable {
    pub fn new() -> PidTable {
        Self::default()
    }

    fn get_entry(&self, pid: pid_t) -> Option<&PidEntry> {
        self.table.get(&pid)
    }

    fn get_entry_mut(&mut self, pid: pid_t) -> &mut PidEntry {
        self.table.entry(pid).or_insert_with(Default::default)
    }

    fn remove_item<F>(&mut self, pid: pid_t, do_remove: F)
    where
        F: FnOnce(&mut PidEntry),
    {
        let entry = self.get_entry_mut(pid);
        do_remove(entry);
        if entry.is_empty() {
            self.table.remove(&pid);
        }
    }

    pub fn set_thread_group_notifier(
        &mut self,
        notifier: std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>,
    ) {
        self.thread_group_notifier = Some(notifier);
    }

    pub fn allocate_pid(&mut self) -> pid_t {
        loop {
            self.last_pid = {
                let r = self.last_pid + 1;
                if r > PID_MAX_LIMIT {
                    track_stub!(TODO("https://fxbug.dev/322874557"), "pid wraparound");
                    2
                } else {
                    r
                }
            };
            if self.get_entry(self.last_pid).is_none() {
                break;
            }
        }
        self.last_pid
    }

    pub fn get_task(&self, tid: tid_t) -> WeakRef<Task> {
        self.get_entry(tid).and_then(|entry| entry.task.clone()).unwrap_or_else(WeakRef::new)
    }

    pub fn add_task(&mut self, task: &TempRef<'_, Task>) {
        let entry = self.get_entry_mut(task.tid);
        assert!(entry.task.is_none());
        self.get_entry_mut(task.tid).task = Some(WeakRef::from(task));
    }

    pub fn remove_task(&mut self, tid: tid_t) {
        self.remove_item(tid, |entry| {
            let removed = entry.task.take();
            assert!(removed.is_some())
        });
    }

    pub fn get_process(&self, pid: pid_t) -> Option<ProcessEntryRef<'_>> {
        match self.get_entry(pid) {
            None => None,
            Some(PidEntry { process: ProcessEntry::None, .. }) => None,
            Some(PidEntry { process: ProcessEntry::ThreadGroup(thread_group), .. }) => {
                let thread_group = thread_group
                    .upgrade()
                    .expect("ThreadGroup was released, but not removed from PidTable");
                Some(ProcessEntryRef::Process(thread_group))
            }
            Some(PidEntry { process: ProcessEntry::Zombie(zombie), .. }) => {
                let zombie = zombie
                    .upgrade()
                    .expect("ZombieProcess was released, but not removed from PidTable");
                Some(ProcessEntryRef::Zombie(zombie))
            }
        }
    }

    pub fn get_thread_group(&self, pid: pid_t) -> Option<TempRef<'_, ThreadGroup>> {
        match self.get_process(pid) {
            Some(ProcessEntryRef::Process(tg)) => Some(tg),
            _ => None,
        }
    }

    pub fn get_thread_groups(&self) -> impl Iterator<Item = TempRef<'_, ThreadGroup>> + '_ {
        self.table
            .iter()
            .flat_map(|(_pid, entry)| entry.process.thread_group())
            .flat_map(|g| g.upgrade())
    }

    pub fn add_thread_group(&mut self, thread_group: &ThreadGroup) {
        let entry = self.get_entry_mut(thread_group.leader);
        assert!(entry.process.is_none());
        entry.process = ProcessEntry::ThreadGroup(thread_group.weak_self.clone());

        // Notify thread group changes.
        if let Some(notifier) = &self.thread_group_notifier {
            thread_group.write().notifier = Some(notifier.clone());
            if thread_group.read().tasks_count() > 0 {
                // We only send the notification if the task group already has an active leader
                // task, ie. its task count is not zero. If it is zero, the task group will send the
                // notification itself once the first task is added.
                let _ =
                    notifier.send(MemoryAttributionLifecycleEvent::creation(thread_group.leader));
            }
        }
    }

    /// Replace process with the specified `pid` with the `zombie`.
    pub fn kill_process(&mut self, pid: pid_t, zombie: WeakRef<ZombieProcess>) {
        let entry = self.get_entry_mut(pid);
        assert!(matches!(entry.process, ProcessEntry::ThreadGroup(_)));

        // All tasks from the process are expected to be cleared from the table before the process
        // becomes a zombie. We can't verify this for all tasks here, check it just for the leader.
        assert!(entry.task.is_none());

        entry.process = ProcessEntry::Zombie(zombie);
    }

    pub fn remove_zombie(&mut self, pid: pid_t) {
        self.remove_item(pid, |entry| {
            assert!(matches!(entry.process, ProcessEntry::Zombie(_)));
            entry.process = ProcessEntry::None;
        });

        // Notify thread group changes.
        if let Some(notifier) = &self.thread_group_notifier {
            let _ = notifier.send(MemoryAttributionLifecycleEvent::destruction(pid));
        }
    }

    pub fn get_process_group(&self, pid: pid_t) -> Option<Arc<ProcessGroup>> {
        self.get_entry(pid)
            .and_then(|entry| entry.process_group.as_ref())
            .and_then(|process_group| process_group.upgrade())
    }

    pub fn add_process_group(&mut self, process_group: &Arc<ProcessGroup>) {
        let entry = self.get_entry_mut(process_group.leader);
        assert!(entry.process_group.is_none());
        entry.process_group = Some(Arc::downgrade(process_group));
    }

    pub fn remove_process_group(&mut self, pid: pid_t) {
        self.remove_item(pid, |entry| {
            let removed = entry.process_group.take();
            assert!(removed.is_some())
        });
    }

    /// Returns the process ids for all processes, including zombies.
    pub fn process_ids(&self) -> Vec<pid_t> {
        self.table
            .iter()
            .flat_map(|(pid, entry)| if entry.process.is_none() { None } else { Some(*pid) })
            .collect()
    }

    /// Returns the task ids for all the currently running tasks.
    pub fn task_ids(&self) -> Vec<pid_t> {
        self.table.iter().flat_map(|(pid, entry)| entry.task.as_ref().and(Some(*pid))).collect()
    }

    pub fn last_pid(&self) -> pid_t {
        self.last_pid
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }
}
