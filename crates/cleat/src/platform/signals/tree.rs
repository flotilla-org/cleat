//! Best-effort Unix descendant and process-group member signaling. Capture before signaling: a dying
//! parent can immediately reparent its children, losing the ancestry link.
use std::collections::{HashMap, HashSet};

use nix::{
    errno::Errno,
    sys::signal::{kill, Signal},
    unistd::{getpgid, Pid},
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    birth: u128,
}

impl ProcessIdentity {
    fn capture(pid: u32) -> Option<Self> {
        process_birth(pid).map(|birth| Self { pid, birth })
    }

    fn capture_in_groups(pid: u32, groups: &[u32]) -> Option<Self> {
        let in_group =
            || getpgid(Some(Pid::from_raw(pid as i32))).is_ok_and(|pgid| pgid.as_raw() > 0 && groups.contains(&(pgid.as_raw() as u32)));
        if groups.is_empty() || !in_group() {
            return None;
        }
        let identity = Self::capture(pid)?;
        // Recheck after capturing identity: the prefilter may have observed a
        // different process if this PID was recycled. The caller validates the
        // birth stamp again after enumeration.
        in_group().then_some(identity)
    }

    fn is_current(self) -> bool {
        process_birth(self.pid) == Some(self.birth)
    }
}

/// Retained independently of the session actor so escalation survives leader
/// exit and reparenting. Birth stamps guard against signaling a reused PID.
/// This is a snapshot, not a containment primitive: a process which reparents
/// before capture can only be discovered if it remains in a captured group.
pub(crate) struct ProcessTree {
    processes: Vec<ProcessIdentity>,
}

/// Capture succeeded and delivery was attempted. A partial delivery error must
/// not discard the identities needed for subsequent escalation.
pub(crate) struct TreeTermination {
    pub(crate) tree: ProcessTree,
    pub(crate) delivery: Result<(), String>,
}

impl ProcessTree {
    #[cfg(test)]
    pub(crate) fn test_snapshot(pids: &[u32]) -> Self {
        Self { processes: pids.iter().map(|&pid| ProcessIdentity { pid, birth: 1 }).collect() }
    }

    pub(crate) fn capture(leader: u32, foreground: Option<u32>) -> Self {
        let groups = [leader, foreground.unwrap_or(leader)];
        Self::from_roots(&ProcessIdentity::capture(leader).into_iter().collect::<Vec<_>>(), &groups)
    }

    fn from_roots(roots: &[ProcessIdentity], groups: &[u32]) -> Self {
        if roots.is_empty() && groups.is_empty() {
            return Self { processes: Vec::new() };
        }
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut roots = roots.to_vec();
        for (pid, process) in system.processes() {
            if let Some(identity) = ProcessIdentity::capture_in_groups(pid.as_u32(), groups) {
                roots.push(identity);
            }
            if let Some(parent) = process.parent() {
                children.entry(parent.as_u32()).or_default().push(pid.as_u32());
            }
        }
        // Validate retained roots after enumeration so a recycled root cannot
        // cause us to adopt an unrelated process's new descendants.
        let roots: HashMap<_, _> = roots.iter().filter(|root| root.is_current()).map(|root| (root.pid, *root)).collect();
        let pids = descendants(&roots.keys().copied().collect::<Vec<_>>(), &children);
        Self { processes: pids.into_iter().filter_map(|pid| roots.get(&pid).copied().or_else(|| ProcessIdentity::capture(pid))).collect() }
    }

    pub(crate) fn signal_outside_groups(&self, signal: Signal, groups: &[u32]) -> Result<(), String> {
        self.signal_with(
            signal,
            |pid, birth| {
                ProcessIdentity { pid, birth }.is_current()
                    && !getpgid(Some(Pid::from_raw(pid as i32))).is_ok_and(|pgid| groups.contains(&(pgid.as_raw() as u32)))
            },
            |pid, signal| kill(Pid::from_raw(pid as i32), signal),
        )
    }

    pub(crate) fn signal_with(
        &self,
        signal: Signal,
        mut eligible: impl FnMut(u32, u128) -> bool,
        mut deliver: impl FnMut(u32, Signal) -> Result<(), Errno>,
    ) -> Result<(), String> {
        let mut error = None;
        // Children first; continue delivering even after a partial failure.
        for process in self.processes.iter().rev() {
            if !eligible(process.pid, process.birth) {
                continue;
            }
            if let Err(err) = deliver(process.pid, signal) {
                if err != Errno::ESRCH {
                    error.get_or_insert_with(|| format!("kill {}: {err}", process.pid));
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    pub(crate) fn kill_survivors(&self) -> Result<(), String> {
        // Include descendants born during grace, including those of surviving
        // escaped children after the original leader has been reaped.
        // Never rediscover members by a retained group ID: it may have been
        // recycled during grace. Only validated identities become new roots.
        Self::from_roots(&self.processes, &[]).signal_outside_groups(Signal::SIGKILL, &[])
    }
}

fn descendants(roots: &[u32], children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut pending = roots.to_vec();
    while let Some(pid) = pending.pop() {
        if seen.insert(pid) {
            result.push(pid);
            if let Some(children) = children.get(&pid) {
                pending.extend(children);
            }
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn process_birth(pid: u32) -> Option<u128> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm is parenthesized and can itself contain spaces and ')'. Field 22
    // is the start time in clock ticks, not sysinfo's rounded seconds.
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn process_birth(pid: u32) -> Option<u128> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: proc_pidinfo writes at most `size` bytes into this valid buffer.
    let read = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if read != size {
        return None;
    }
    // SAFETY: a successful full-sized response initialized the structure.
    let info = unsafe { info.assume_init() };
    Some(u128::from(info.pbi_start_tvsec) * 1_000_000 + u128::from(info.pbi_start_tvusec))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_birth(pid: u32) -> Option<u128> {
    let mut system = System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing());
    system.process(pid).map(|process| u128::from(process.start_time()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_includes_nested_and_overlapping_roots_only_once() {
        let children = HashMap::from([(1, vec![2, 3]), (2, vec![4]), (4, vec![2]), (99, vec![100])]);
        let mut actual = descendants(&[1, 2], &children);
        actual.sort_unstable();
        assert_eq!(actual, vec![1, 2, 3, 4]);
    }

    #[test]
    fn captures_group_members_without_a_live_ancestry_root() {
        let identity = ProcessIdentity::capture(std::process::id()).unwrap();
        let pgid = getpgid(None).unwrap().as_raw() as u32;
        let tree = ProcessTree::capture(u32::MAX, Some(pgid));
        assert!(tree.processes.contains(&identity));
        assert_eq!(tree.processes.iter().filter(|process| **process == identity).count(), 1);
    }

    #[test]
    fn stale_birth_stamp_is_not_signaled() {
        let mut identity = ProcessIdentity::capture(std::process::id()).unwrap();
        identity.birth += 1;
        // Would kill the test runner if identity validation were omitted.
        let tree = ProcessTree { processes: vec![identity] };
        tree.signal_outside_groups(Signal::SIGKILL, &[]).unwrap();
        tree.kill_survivors().unwrap();
    }
}
