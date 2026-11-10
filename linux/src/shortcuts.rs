use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const ACTIONS: [&str; 5] = ["toggle", "search", "library", "copyLatest", "screenshot"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedShortcut {
    pub action: String,
    pub shortcut: String,
    pub previous: String,
    #[serde(
        rename = "restorationFailed",
        skip_serializing_if = "Option::is_none"
    )]
    pub restoration_failed: Option<bool>,
}

pub trait ShortcutRegistry {
    fn register(&mut self, key: &str, action: &str) -> bool;
    fn unregister(&mut self, key: &str);
}

pub struct GlobalShortcuts<R: ShortcutRegistry> {
    pub registry: R,
    pub registered: HashMap<String, String>,
    pub active: HashSet<String>,
    pub epochs: HashMap<String, u64>,
    pub leases: HashSet<u64>,
    pub disposed: bool,
    next_epoch: u64,
    next_lease: u64,
    failed: Box<dyn Fn(&str) + Send + Sync>,
}

impl<R: ShortcutRegistry> GlobalShortcuts<R> {
    pub fn new(registry: R, failed: Box<dyn Fn(&str) + Send + Sync>) -> Self {
        Self {
            registry,
            registered: HashMap::new(),
            active: HashSet::new(),
            epochs: HashMap::new(),
            leases: HashSet::new(),
            disposed: false,
            next_epoch: 1,
            next_lease: 1,
            failed,
        }
    }

    pub fn suspended(&self) -> bool {
        !self.leases.is_empty()
    }

    fn do_register(&mut self, action: &str, key: &str) -> bool {
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        let success = self.registry.register(key, action);
        if success {
            self.epochs.insert(action.to_string(), epoch);
        }
        success
    }

    pub fn suspend(&mut self) -> Result<u64, String> {
        if self.disposed {
            return Err("Shortcut registrations are closed.".to_string());
        }
        let token = self.next_lease;
        self.next_lease += 1;
        self.leases.insert(token);
        if self.leases.len() == 1 {
            let entries: Vec<(String, String)> = self
                .registered
                .iter()
                .map(|(a, k)| (a.clone(), k.clone()))
                .collect();
            for (action, key) in entries {
                if self.active.contains(&action) {
                    self.registry.unregister(&key);
                    self.active.remove(&action);
                    self.epochs.remove(&action);
                }
            }
        }
        Ok(token)
    }

    pub fn resume(&mut self, token: u64) {
        if !self.leases.remove(&token) || self.suspended() || self.disposed {
            return;
        }
        let entries: Vec<(String, String)> = self
            .registered
            .iter()
            .map(|(a, k)| (a.clone(), k.clone()))
            .collect();
        for (action, key) in entries {
            if self.active.contains(&action) {
                continue;
            }
            if self.do_register(&action, &key) {
                self.active.insert(action);
            } else {
                self.registered.remove(&action);
                (self.failed)(
                    "A shortcut could not be restored after recording. Choose another combination in Settings.",
                );
            }
        }
    }

    pub fn update(&mut self, shortcuts: &HashMap<String, String>) -> Vec<RejectedShortcut> {
        let mut rejected = Vec::new();
        let mut swapped = HashSet::new();

        if !self.suspended() && !self.disposed {
            for &action in &ACTIONS {
                let next = shortcuts.get(action).map(|s| s.as_str()).unwrap_or("");
                if !next.is_empty() || !self.registered.contains_key(action) {
                    continue;
                }
                if let Some(key) = self.registered.remove(action) {
                    if self.active.contains(action) {
                        self.registry.unregister(&key);
                    }
                    self.active.remove(action);
                    self.epochs.remove(action);
                }
            }
        }

        // >= 3-way circular permutation
        if !self.suspended() && !self.disposed {
            for &action in &ACTIONS {
                if swapped.contains(action) || !self.active.contains(action) {
                    continue;
                }
                #[derive(Clone)]
                struct CycleStep {
                    action: String,
                    previous: String,
                    next: String,
                }
                let mut cycle = Vec::new();
                let mut visited = HashSet::new();
                let mut cursor = Some(action.to_string());
                while let Some(ref cur) = cursor {
                    if visited.contains(cur) || swapped.contains(cur.as_str()) {
                        break;
                    }
                    visited.insert(cur.clone());
                    let prev = self.registered.get(cur).cloned().unwrap_or_default();
                    let next = shortcuts.get(cur).cloned().unwrap_or_default();
                    if prev.is_empty()
                        || next.is_empty()
                        || next == prev
                        || !self.active.contains(cur)
                    {
                        cursor = None;
                        break;
                    }
                    cycle.push(CycleStep {
                        action: cur.clone(),
                        previous: prev,
                        next: next.clone(),
                    });
                    cursor = ACTIONS
                        .iter()
                        .find(|&&cand| {
                            self.active.contains(cand)
                                && self.registered.get(cand).map(|s| s.as_str())
                                    == Some(next.as_str())
                        })
                        .map(|&s| s.to_string());
                }

                if cursor.as_deref() != Some(action) || cycle.len() < 3 {
                    continue;
                }

                let mut installed: Vec<CycleStep> = Vec::new();
                let mut success = true;
                for item in &cycle {
                    self.registry.unregister(&item.previous);
                    self.active.remove(&item.action);
                    self.epochs.remove(&item.action);
                }
                for item in &cycle {
                    if !self.do_register(&item.action, &item.next) {
                        success = false;
                        break;
                    }
                    self.active.insert(item.action.clone());
                    installed.push(item.clone());
                }

                if success {
                    for item in &cycle {
                        self.registered
                            .insert(item.action.clone(), item.next.clone());
                    }
                } else {
                    for item in &installed {
                        self.registry.unregister(&item.next);
                        self.active.remove(&item.action);
                        self.epochs.remove(&item.action);
                    }
                    let mut restoration_failed = HashSet::new();
                    for item in &cycle {
                        if self.active.contains(&item.action) {
                            continue;
                        }
                        if self.do_register(&item.action, &item.previous) {
                            self.active.insert(item.action.clone());
                        } else {
                            self.registered.remove(&item.action);
                            restoration_failed.insert(item.action.clone());
                            (self.failed)("A shortcut could not be restored after a failed rotation.");
                        }
                    }
                    for item in &cycle {
                        rejected.push(RejectedShortcut {
                            action: item.action.clone(),
                            shortcut: item.next.clone(),
                            previous: item.previous.clone(),
                            restoration_failed: if restoration_failed.contains(&item.action) {
                                Some(true)
                            } else {
                                None
                            },
                        });
                    }
                }
                for item in &cycle {
                    swapped.insert(item.action.clone());
                }
            }
        }

        // 2-way reciprocal swap
        if !self.suspended() && !self.disposed {
            for &action in &ACTIONS {
                if swapped.contains(action) {
                    continue;
                }
                let previous = self.registered.get(action).cloned().unwrap_or_default();
                let next = shortcuts.get(action).cloned().unwrap_or_default();
                if previous.is_empty()
                    || next.is_empty()
                    || next == previous
                    || !self.active.contains(action)
                {
                    continue;
                }
                let other_opt = ACTIONS.iter().copied().find(|&cand| {
                    cand != action
                        && self.active.contains(cand)
                        && self.registered.get(cand).map(|s| s.as_str()) == Some(next.as_str())
                        && shortcuts.get(cand).map(|s| s.as_str()).unwrap_or("")
                            == previous.as_str()
                });
                let Some(other) = other_opt else {
                    continue;
                };
                let pair = [
                    (action.to_string(), previous.clone(), next.clone()),
                    (other.to_string(), next.clone(), previous.clone()),
                ];
                let mut installed = Vec::new();
                let mut success = true;
                for (act, prev, _) in &pair {
                    self.registry.unregister(prev);
                    self.active.remove(act);
                    self.epochs.remove(act);
                }
                for (act, _, nxt) in &pair {
                    if !self.do_register(act, nxt) {
                        success = false;
                        break;
                    }
                    self.active.insert(act.clone());
                    installed.push((act.clone(), nxt.clone()));
                }
                if success {
                    for (act, _, nxt) in &pair {
                        self.registered.insert(act.clone(), nxt.clone());
                    }
                } else {
                    for (act, nxt) in &installed {
                        self.registry.unregister(nxt);
                        self.active.remove(act);
                        self.epochs.remove(act);
                    }
                    for (act, prev, _) in &pair {
                        if self.active.contains(act) {
                            continue;
                        }
                        if self.do_register(act, prev) {
                            self.active.insert(act.clone());
                        } else {
                            self.registered.remove(act);
                            (self.failed)("A shortcut could not be restored after a failed swap.");
                        }
                    }
                    for (act, prev, nxt) in &pair {
                        rejected.push(RejectedShortcut {
                            action: act.clone(),
                            shortcut: nxt.clone(),
                            previous: prev.clone(),
                            restoration_failed: None,
                        });
                    }
                }
                swapped.insert(action.to_string());
                swapped.insert(other.to_string());
            }
        }

        // Individual updates
        for &action in &ACTIONS {
            if swapped.contains(action) {
                continue;
            }
            let next = shortcuts.get(action).cloned().unwrap_or_default();
            let previous = self.registered.get(action).cloned().unwrap_or_default();
            if next == previous {
                continue;
            }
            if self.suspended() || self.disposed {
                rejected.push(RejectedShortcut {
                    action: action.to_string(),
                    shortcut: next,
                    previous,
                    restoration_failed: None,
                });
                continue;
            }
            if next.is_empty() {
                if !previous.is_empty() && self.active.contains(action) {
                    self.registry.unregister(&previous);
                }
                self.active.remove(action);
                self.epochs.remove(action);
                self.registered.remove(action);
                continue;
            }
            if self.do_register(action, &next) {
                if !previous.is_empty() && self.active.contains(action) {
                    self.registry.unregister(&previous);
                }
                self.registered.insert(action.to_string(), next);
                self.active.insert(action.to_string());
            } else {
                rejected.push(RejectedShortcut {
                    action: action.to_string(),
                    shortcut: next,
                    previous,
                    restoration_failed: None,
                });
            }
        }

        rejected
    }

    pub fn dispose(&mut self) {
        self.disposed = true;
        self.leases.clear();
        let entries: Vec<(String, String)> = self
            .registered
            .iter()
            .map(|(a, k)| (a.clone(), k.clone()))
            .collect();
        for (action, key) in entries {
            if self.active.contains(&action) {
                self.registry.unregister(&key);
            }
        }
        self.active.clear();
        self.epochs.clear();
        self.registered.clear();
    }
}
