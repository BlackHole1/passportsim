//! The FreeRTOS task and lock walker.
//!
//! Every ready list, both delayed lists, and the pending-ready, suspended and waiting-termination
//! lists are walked; each item's `pvOwner` is a TCB, the list gives the state, and
//! `pxCurrentTCBs[0]` is running. The stack high-water mark is the run of `tskSTACK_FILL_BYTE`
//! (0xA5) from `pxStack` upward (`StackType_t` is `uint8_t` on this port).
//!
//! A list whose links disagree with its item count, or whose items name another container, is
//! **reported and abandoned**, never followed: chasing a corrupted chain in a crashed guest would
//! make the introspection tool the second bug.

use crate::GuestMemory;
use crate::layout::Layouts;
use crate::{IntrospectError, Warning};
use pemu_loader::symbols::SymbolTable;

/// Longest run of list links followed before declaring the list unbounded. FreeRTOS lists hold
/// one entry per task, so a longer chain is corruption whatever `uxNumberOfItems` claims.
pub const MAX_LIST_ITEMS: u32 = 256;

/// `pcTaskName[16]`, with room for a missing terminator.
const MAX_NAME: usize = 16;

pub const STACK_FILL_BYTE: u8 = 0xA5;

/// Which list a task was found in.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TaskState {
    /// The TCB `pxCurrentTCBs[0]` points at.
    Running,
    Ready,
    /// In a delayed list, or waiting indefinitely in the suspended list.
    Blocked,
    /// In `xSuspendedTaskList`.
    Suspended,
    /// In `xPendingReadyList`.
    PendingReady,
    /// In `xTasksWaitingTermination`.
    Deleted,
}

impl TaskState {
    pub fn tag(self) -> &'static str {
        match self {
            TaskState::Running => "running",
            TaskState::Ready => "ready",
            TaskState::Blocked => "blocked",
            TaskState::Suspended => "suspended",
            TaskState::PendingReady => "pending-ready",
            TaskState::Deleted => "deleted",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Task {
    pub tcb: u32,
    pub name: String,
    pub state: TaskState,
    /// `uxPriority`, which mutex priority inheritance raises.
    pub priority: u32,
    /// `uxBasePriority`, the priority the task was created with.
    pub base_priority: u32,
    /// `pxStack`, the low address.
    pub stack_base: u32,
    /// `pxEndOfStack`, the highest usable byte, so the stack spans `end - base + 1` bytes.
    pub stack_end: u32,
    pub stack_bytes: u32,
    /// Unused bytes still holding [`STACK_FILL_BYTE`].
    pub stack_free_bytes: u32,
    pub top_of_stack: u32,
    /// `xEventListItem.pxContainer`, 0 when none.
    pub event_list: u32,
    /// What that event list belongs to, once [`resolve_blocks`] has named it.
    pub blocked_on: Option<String>,
    /// The wait has no timeout: the task was in `xSuspendedTaskList` waiting on an event list or
    /// a notification, where `prvAddCurrentTaskToDelayedList` parks a `portMAX_DELAY` wait when
    /// `INCLUDE_vTaskSuspend` is 1 (the ESP-IDF default). Only such a wait can be part of a deadlock.
    /// With `INCLUDE_vTaskSuspend` 0 it is never set, so no cycle is reported: the safe direction.
    pub indefinite: bool,
    /// Set when [`resolve_mutex_waits`] confirmed one.
    pub mutex_wait: Option<MutexWait>,
}

/// A blocked task's wait, confirmed to be a mutex another task holds: the `QueueDefinition`
/// has `pcHead == NULL` (`queueQUEUE_IS_MUTEX`), and its `xMutexHolder` is another walked task.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MutexWait {
    pub queue: u32,
    pub holder: u32,
    pub recursive_count: u32,
    /// On `xTasksWaitingToSend` rather than `xTasksWaitingToReceive`.
    pub to_send: bool,
}

impl MutexWait {
    #[must_use]
    pub fn how(&self) -> &'static str {
        if self.to_send {
            "waiting to send"
        } else {
            "waiting to receive"
        }
    }
}

impl Task {
    /// `<name> <state> prio=<p>(<base>) stack=<free>/<total> tcb=<addr>`.
    pub fn render(&self) -> String {
        let mut out = format!(
            "{} {} prio={}({}) stack={}/{} tcb={:#010x}",
            self.name,
            self.state.tag(),
            self.priority,
            self.base_priority,
            self.stack_free_bytes,
            self.stack_bytes,
            self.tcb
        );
        match (&self.blocked_on, self.event_list) {
            (Some(what), _) => out.push_str(&format!(" blocked on {what}")),
            (None, 0) => {}
            (None, list) => out.push_str(&format!(" blocked on event list {list:#010x}")),
        }
        out
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TaskSnapshot {
    /// In TCB address order, so two walks of the same memory render identically.
    pub tasks: Vec<Task>,
    pub tick: Option<u32>,
    pub count: Option<u32>,
    pub current: Option<u32>,
    /// Idle task TCBs from the scheduler's own `xIdleTaskHandle` (`xIdleTaskHandles` in SMP),
    /// not guessed from a priority.
    pub idle: Vec<u32>,
    pub warnings: Vec<Warning>,
}

impl TaskSnapshot {
    pub fn render(&self) -> String {
        let mut out = String::new();
        for t in &self.tasks {
            out.push_str(&t.render());
            out.push('\n');
        }
        for w in &self.warnings {
            out.push_str(&format!("warning: {w}\n"));
        }
        out
    }

    pub fn task(&self, tcb: u32) -> Option<&Task> {
        self.tasks.iter().find(|t| t.tcb == tcb)
    }

    /// The walk found exactly the tasks the scheduler counts and has nothing to report.
    pub fn is_consistent(&self) -> bool {
        self.warnings.is_empty() && self.count.is_none_or(|c| c as usize == self.tasks.len())
    }
}

pub(crate) fn symbol(syms: &SymbolTable, name: &'static str) -> Result<u32, IntrospectError> {
    syms.addr_of(name)
        .ok_or(IntrospectError::MissingSymbol { name })
}

struct ListSource {
    at: u32,
    state: TaskState,
    what: &'static str,
}

/// Walks every FreeRTOS task list. The ready-list count is the size of the
/// `pxReadyTasksLists` symbol over `sizeof(xLIST)`, so any `configMAX_PRIORITIES` works.
pub fn walk_tasks(
    layouts: &Layouts,
    syms: &SymbolTable,
    mem: &dyn GuestMemory,
) -> Result<TaskSnapshot, IntrospectError> {
    walk_tasks_with(layouts, syms, mem, WalkOptions::default())
}

/// What a walk reads beyond the lists and TCBs. The stack high-water mark reads every stack byte
/// (tens of thousands of guest reads), so the per-slice deadlock watch turns it off.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct WalkOptions {
    /// With this off, [`Task::stack_free_bytes`] is 0, which is not the mark.
    pub stack_high_water: bool,
}

impl Default for WalkOptions {
    fn default() -> WalkOptions {
        WalkOptions {
            stack_high_water: true,
        }
    }
}

pub fn walk_tasks_with(
    layouts: &Layouts,
    syms: &SymbolTable,
    mem: &dyn GuestMemory,
    opts: WalkOptions,
) -> Result<TaskSnapshot, IntrospectError> {
    let list = layouts.require("xLIST")?;
    let item = layouts.require("xLIST_ITEM")?;
    let tcb = layouts.require("tskTaskControlBlock")?;
    let mut out = TaskSnapshot {
        tick: syms.addr_of("xTickCount").and_then(|a| mem.u32(a).ok()),
        count: syms
            .addr_of("uxCurrentNumberOfTasks")
            .and_then(|a| mem.u32(a).ok()),
        current: syms.addr_of("pxCurrentTCBs").and_then(|a| mem.u32(a).ok()),
        idle: idle_handles(syms, mem),
        ..TaskSnapshot::default()
    };

    let mut sources: Vec<ListSource> = Vec::new();
    let ready = syms
        .lookup("pxReadyTasksLists")
        .ok_or(IntrospectError::MissingSymbol {
            name: "pxReadyTasksLists",
        })?;
    let priorities = if list.size == 0 {
        0
    } else {
        ready.size / list.size
    };
    // A symbol with no usable size would otherwise silently drop every ready task,
    // the running one included.
    if priorities == 0 {
        out.warnings.push(Warning {
            what: "pxReadyTasksLists",
            at: ready.addr,
            detail: format!(
                "its symbol size {} holds no whole xLIST of {}, so no ready list is walked",
                ready.size, list.size
            ),
        });
    } else if ready.size % list.size != 0 {
        out.warnings.push(Warning {
            what: "pxReadyTasksLists",
            at: ready.addr,
            detail: format!(
                "its symbol size {} is not a whole number of xLIST of {}, \
                 so only {priorities} ready lists are walked",
                ready.size, list.size
            ),
        });
    }
    for p in 0..priorities {
        sources.push(ListSource {
            at: ready.addr + p * list.size,
            state: TaskState::Ready,
            what: "pxReadyTasksLists",
        });
    }
    // The two delayed lists are reached through pointers, which swap on tick overflow.
    for name in ["pxDelayedTaskList", "pxOverflowDelayedTaskList"] {
        if let Some(at) = syms.addr_of(name).and_then(|a| mem.u32(a).ok())
            && at != 0
        {
            sources.push(ListSource {
                at,
                state: TaskState::Blocked,
                what: "delayed task list",
            });
        }
    }
    for (name, state) in [
        ("xPendingReadyList", TaskState::PendingReady),
        ("xSuspendedTaskList", TaskState::Suspended),
        ("xTasksWaitingTermination", TaskState::Deleted),
    ] {
        if let Some(at) = syms.addr_of(name) {
            sources.push(ListSource {
                at,
                state,
                what: "task list",
            });
        }
    }

    for source in &sources {
        let owners = walk_list(layouts, mem, source.at, source.what, &mut out.warnings);
        for owner in owners {
            if out.tasks.iter().any(|t| t.tcb == owner) {
                continue;
            }
            match read_task(tcb, item, mem, owner, source.state, opts) {
                // Like `eTaskGetState`: a task in `xSuspendedTaskList` still waiting on an event
                // list or a notification is blocked, parked there by a `portMAX_DELAY` wait.
                Ok(mut task) => {
                    if task.state == TaskState::Suspended
                        && (task.event_list != 0
                            || tcb.u8(mem, owner, "ucNotifyState").ok()
                                == Some(TASK_WAITING_NOTIFICATION))
                    {
                        task.state = TaskState::Blocked;
                        task.indefinite = true;
                    }
                    out.tasks.push(task)
                }
                Err(e) => out.warnings.push(Warning {
                    what: "TCB",
                    at: owner,
                    detail: e.to_string(),
                }),
            }
        }
    }
    if let Some(current) = out.current.filter(|c| *c != 0) {
        match out.tasks.iter_mut().find(|t| t.tcb == current) {
            Some(task) => task.state = TaskState::Running,
            None => out.warnings.push(Warning {
                what: "pxCurrentTCBs",
                at: current,
                detail: "the running task is in none of the walked lists".into(),
            }),
        }
    }
    out.tasks.sort_by_key(|t| t.tcb);
    Ok(out)
}

/// One per core; the C3 has one.
const MAX_IDLE_TASKS: u32 = 4;

/// Idle task TCBs from `xIdleTaskHandle` (single-core) or `xIdleTaskHandles` (SMP), as many as
/// the symbol size holds. A zero handle (idle task not created yet) is left out.
fn idle_handles(syms: &SymbolTable, mem: &dyn GuestMemory) -> Vec<u32> {
    let mut out = Vec::new();
    for name in ["xIdleTaskHandle", "xIdleTaskHandles"] {
        let Some(sym) = syms.lookup(name) else {
            continue;
        };
        for n in 0..(sym.size / 4).clamp(1, MAX_IDLE_TASKS) {
            if let Ok(tcb) = mem.u32(sym.addr + n * 4)
                && tcb != 0
                && !out.contains(&tcb)
            {
                out.push(tcb);
            }
        }
    }
    out
}

/// `taskWAITING_NOTIFICATION` of FreeRTOS `tasks.c`.
const TASK_WAITING_NOTIFICATION: u8 = 1;

/// Walks one `xLIST` from `xListEnd.pxNext` back to `xListEnd`, returning each item's
/// `pvOwner`. Stops with a warning on an unreadable link, a foreign container, a revisited item,
/// or a run past `uxNumberOfItems` or [`MAX_LIST_ITEMS`].
pub fn walk_list(
    layouts: &Layouts,
    mem: &dyn GuestMemory,
    at: u32,
    what: &'static str,
    warnings: &mut Vec<Warning>,
) -> Vec<u32> {
    let mut out = Vec::new();
    let (Ok(list), Ok(item)) = (layouts.require("xLIST"), layouts.require("xLIST_ITEM")) else {
        return out;
    };
    let (Ok(claimed), Ok(end_off)) = (
        list.u32(mem, at, "uxNumberOfItems"),
        list.offset("xListEnd"),
    ) else {
        warnings.push(Warning {
            what,
            at,
            detail: "the list header is not readable".into(),
        });
        return out;
    };
    let end = at.wrapping_add(end_off);
    // In the end marker, `pxNext` sits at the same offset as in a full item.
    let Ok(next_off) = item.offset("pxNext") else {
        return out;
    };
    if claimed > MAX_LIST_ITEMS {
        warnings.push(Warning {
            what,
            at,
            detail: format!("uxNumberOfItems is {claimed}, past the {MAX_LIST_ITEMS} cap"),
        });
        return out;
    }
    let mut cursor = match mem.u32(end.wrapping_add(next_off)) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(Warning {
                what,
                at: end,
                detail: e.to_string(),
            });
            return out;
        }
    };
    let mut seen = 0u32;
    while cursor != end {
        if seen >= claimed {
            warnings.push(Warning {
                what,
                at: cursor,
                detail: format!(
                    "the links run past uxNumberOfItems ({claimed}); the list is not followed \
                     further"
                ),
            });
            return out;
        }
        let (owner, container, next) = match (
            item.u32(mem, cursor, "pvOwner"),
            item.u32(mem, cursor, "pxContainer"),
            mem.u32(cursor.wrapping_add(next_off)),
        ) {
            (Ok(o), Ok(c), Ok(n)) => (o, c, n),
            _ => {
                warnings.push(Warning {
                    what,
                    at: cursor,
                    detail: "the list item is not readable".into(),
                });
                return out;
            }
        };
        if container != at {
            warnings.push(Warning {
                what,
                at: cursor,
                detail: format!(
                    "pxContainer is {container:#010x}, not this list; the list is not followed \
                     further"
                ),
            });
            return out;
        }
        if out.contains(&owner) {
            warnings.push(Warning {
                what,
                at: cursor,
                detail: "the links revisit an item, so the list is a cycle".into(),
            });
            return out;
        }
        out.push(owner);
        seen += 1;
        cursor = next;
    }
    if seen != claimed {
        warnings.push(Warning {
            what,
            at,
            detail: format!("uxNumberOfItems is {claimed} but the links hold {seen}"),
        });
    }
    out
}

fn read_task(
    tcb: &crate::layout::StructLayout,
    item: &crate::layout::StructLayout,
    mem: &dyn GuestMemory,
    at: u32,
    state: TaskState,
    opts: WalkOptions,
) -> Result<Task, IntrospectError> {
    let stack_base = tcb.u32(mem, at, "pxStack")?;
    let stack_end = tcb.u32(mem, at, "pxEndOfStack")?;
    let stack_bytes = stack_end.saturating_sub(stack_base).saturating_add(1);
    let event_item = at.wrapping_add(tcb.offset("xEventListItem")?);
    Ok(Task {
        tcb: at,
        name: mem.cstr(at.wrapping_add(tcb.offset("pcTaskName")?), MAX_NAME),
        state,
        priority: tcb.u32(mem, at, "uxPriority")?,
        base_priority: tcb.u32(mem, at, "uxBasePriority")?,
        stack_base,
        stack_end,
        stack_bytes,
        stack_free_bytes: if opts.stack_high_water {
            fill_run(mem, stack_base, stack_bytes)
        } else {
            0
        },
        top_of_stack: tcb.u32(mem, at, "pxTopOfStack")?,
        event_list: item.u32(mem, event_item, "pxContainer").unwrap_or(0),
        blocked_on: None,
        indefinite: false,
        mutex_wait: None,
    })
}

/// The run of [`STACK_FILL_BYTE`] from `base` upward. An unreadable byte ends the count.
fn fill_run(mem: &dyn GuestMemory, base: u32, len: u32) -> u32 {
    let mut n = 0;
    while n < len {
        match mem.u8(base.wrapping_add(n)) {
            Ok(STACK_FILL_BYTE) => n += 1,
            _ => break,
        }
    }
    n
}

/// A FreeRTOS mutex or semaphore, read through the `QueueDefinition` layout.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MutexInfo {
    pub queue: u32,
    /// `None` when free.
    pub holder: Option<u32>,
    pub holder_name: Option<String>,
    pub recursive_count: u32,
    /// Address of `xTasksWaitingToReceive`, the event list a task blocks on.
    pub waiting_to_receive: u32,
    pub waiting_to_send: u32,
}

impl MutexInfo {
    pub fn read(
        layouts: &Layouts,
        mem: &dyn GuestMemory,
        queue: u32,
        tasks: Option<&TaskSnapshot>,
    ) -> Result<MutexInfo, IntrospectError> {
        let q = layouts.require("QueueDefinition")?;
        let holder = q.u32(mem, queue, "u.xSemaphore.xMutexHolder")?;
        Ok(MutexInfo {
            queue,
            holder: (holder != 0).then_some(holder),
            holder_name: tasks
                .and_then(|t| t.task(holder))
                .map(|t| t.name.clone())
                .filter(|_| holder != 0),
            recursive_count: q.u32(mem, queue, "u.xSemaphore.uxRecursiveCallCount")?,
            waiting_to_receive: queue.wrapping_add(q.offset("xTasksWaitingToReceive")?),
            waiting_to_send: queue.wrapping_add(q.offset("xTasksWaitingToSend")?),
        })
    }

    pub fn render(&self, name: &str) -> String {
        match (&self.holder, &self.holder_name) {
            (None, _) => format!("{name} free"),
            (Some(tcb), Some(task)) => format!(
                "{name} held by {task} ({tcb:#010x}) recursive={}",
                self.recursive_count
            ),
            (Some(tcb), None) => format!(
                "{name} held by {tcb:#010x} recursive={}",
                self.recursive_count
            ),
        }
    }
}

/// Names what each blocked task waits on by matching `xEventListItem.pxContainer` against the
/// wait lists of the caller's `(name, QueueDefinition address)` queues.
pub fn resolve_blocks(
    snapshot: &mut TaskSnapshot,
    layouts: &Layouts,
    mem: &dyn GuestMemory,
    queues: &[(String, u32)],
) {
    let mut known: Vec<(u32, String)> = Vec::new();
    for (name, at) in queues {
        if let Ok(m) = MutexInfo::read(layouts, mem, *at, None) {
            known.push((m.waiting_to_receive, format!("{name} (waiting to receive)")));
            known.push((m.waiting_to_send, format!("{name} (waiting to send)")));
        }
    }
    for task in &mut snapshot.tasks {
        if task.event_list == 0 {
            continue;
        }
        task.blocked_on = known
            .iter()
            .find(|(at, _)| *at == task.event_list)
            .map(|(_, label)| label.clone());
    }
}

/// Names waits on a queue or mutex some walked task holds, and records confirmed mutex waits in
/// [`Task::mutex_wait`]. `pxContainer` minus the wait list's offset is the `QueueDefinition`. A
/// [`MutexWait`] also needs `pcHead == NULL`, because in a plain queue the holder word is
/// `pcReadFrom`; a layout without `pcHead` confirms none.
pub fn resolve_mutex_waits(snapshot: &mut TaskSnapshot, layouts: &Layouts, mem: &dyn GuestMemory) {
    let Ok(queue) = layouts.require("QueueDefinition") else {
        return;
    };
    let head = queue.offset("pcHead").ok();
    let lists = [
        (false, queue.offset("xTasksWaitingToReceive")),
        (true, queue.offset("xTasksWaitingToSend")),
    ];
    let found: Vec<(usize, String, Option<MutexWait>)> = snapshot
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.blocked_on.is_none() && task.event_list != 0)
        .filter_map(|(index, task)| {
            lists.iter().find_map(|(to_send, offset)| {
                let at = task.event_list.wrapping_sub(*offset.as_ref().ok()?);
                let info = MutexInfo::read(layouts, mem, at, Some(snapshot)).ok()?;
                let holder = info.holder?;
                let holder_name = info.holder_name.clone()?;
                let wait = MutexWait {
                    queue: at,
                    holder,
                    recursive_count: info.recursive_count,
                    to_send: *to_send,
                };
                let is_mutex = head
                    .is_some_and(|off| mem.u32(at.wrapping_add(off)).is_ok_and(|head| head == 0))
                    && holder != task.tcb;
                Some((
                    index,
                    format!(
                        "mutex {at:#010x} held by {holder_name} ({}, recursive={})",
                        wait.how(),
                        info.recursive_count
                    ),
                    is_mutex.then_some(wait),
                ))
            })
        })
        .collect();
    for (index, label, wait) in found {
        snapshot.tasks[index].blocked_on = Some(label);
        snapshot.tasks[index].mutex_wait = wait;
    }
}

/// One task of a deadlock cycle, waiting forever for a mutex the next task holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CycleLink {
    pub tcb: u32,
    pub task: String,
    pub mutex: u32,
    pub holder: u32,
    pub holder_name: String,
}

impl CycleLink {
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{} waits for mutex {:#010x} held by {}",
            self.task, self.mutex, self.holder_name
        )
    }
}

/// A task-level deadlock: a mutex cycle with nothing outside it scheduled to run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeadlockReport {
    /// Starting at its lowest TCB, so two walks of the same memory report it identically.
    pub cycle: Vec<CycleLink>,
}

impl DeadlockReport {
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for link in &self.cycle {
            out.push_str(&link.render());
            out.push('\n');
        }
        out
    }
}

/// The mutex cycle in this snapshot: every wait indefinite, each for a mutex the next task
/// holds. Priority inheritance moves priorities, never the holder, so it never resolves. A timed
/// wait, an unconfirmed mutex, or an inconsistent snapshot (possibly fault-damaged memory) yields
/// none.
#[must_use]
pub fn mutex_cycle(snapshot: &TaskSnapshot) -> Option<Vec<CycleLink>> {
    if !snapshot.is_consistent() {
        return None;
    }
    // One edge per task, so a cycle is found by following the chain.
    let edge = |task: &Task| -> Option<(u32, MutexWait)> {
        let wait = task.mutex_wait?;
        (task.state == TaskState::Blocked && task.indefinite)
            .then(|| snapshot.task(wait.holder).map(|holder| (holder.tcb, wait)))
            .flatten()
    };
    let mut best: Option<Vec<u32>> = None;
    for start in snapshot.tasks.iter().filter(|task| edge(task).is_some()) {
        let mut path: Vec<u32> = Vec::new();
        let mut at = start.tcb;
        while let Some(task) = snapshot.task(at)
            && let Some((next, _)) = edge(task)
        {
            if let Some(from) = path.iter().position(|seen| *seen == at) {
                // Lowest TCB first, so the cycle renders the same whichever task reached it,
                // and the lowest cycle wins when there are two.
                let cycle = rotate_to_lowest(path.split_off(from));
                if best.as_ref().is_none_or(|b| cycle < *b) {
                    best = Some(cycle);
                }
                break;
            }
            path.push(at);
            at = next;
            if path.len() > snapshot.tasks.len() {
                break;
            }
        }
    }
    let cycle = best?;
    let links = cycle
        .iter()
        .filter_map(|tcb| {
            let task = snapshot.task(*tcb)?;
            let (_, wait) = edge(task)?;
            let holder = snapshot.task(wait.holder)?;
            Some(CycleLink {
                tcb: task.tcb,
                task: task.name.clone(),
                mutex: wait.queue,
                holder: holder.tcb,
                holder_name: holder.name.clone(),
            })
        })
        .collect::<Vec<_>>();
    (links.len() == cycle.len()).then_some(links)
}

fn rotate_to_lowest(mut cycle: Vec<u32>) -> Vec<u32> {
    if let Some(at) = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, tcb)| **tcb)
        .map(|(at, _)| at)
    {
        cycle.rotate_left(at);
    }
    cycle
}

/// Tasks outside the cycle that can still run: running, ready, pending-ready, or blocked with a
/// timeout. The idle task is exempt, named by [`TaskSnapshot::idle`]; without an idle handle it
/// falls back to priority 0, which at worst suppresses a report.
#[must_use]
pub fn runnable_outside(snapshot: &TaskSnapshot, cycle: &[CycleLink]) -> Vec<String> {
    let idle = |task: &Task| match snapshot.idle.as_slice() {
        [] => task.base_priority == 0,
        known => known.contains(&task.tcb),
    };
    snapshot
        .tasks
        .iter()
        .filter(|task| !cycle.iter().any(|link| link.tcb == task.tcb))
        .filter(|task| match task.state {
            TaskState::Running | TaskState::Ready | TaskState::PendingReady => !idle(task),
            TaskState::Blocked => !task.indefinite,
            TaskState::Suspended | TaskState::Deleted => false,
        })
        .map(|task| task.name.clone())
        .collect()
}

/// The task-level deadlock: a [`mutex_cycle`] with nothing outside it able to run. Both halves
/// matter: `probes/probe_deadlock` closes its cycle while `app_main` is still in a `vTaskDelay`.
#[must_use]
pub fn deadlock_report(snapshot: &TaskSnapshot) -> Option<DeadlockReport> {
    let cycle = mutex_cycle(snapshot)?;
    runnable_outside(snapshot, &cycle)
        .is_empty()
        .then_some(DeadlockReport { cycle })
}

#[cfg(test)]
pub(crate) mod synthetic {
    //! A synthetic FreeRTOS image with the layouts the official ELF resolves to.

    use super::*;
    use crate::MemoryImage;
    use crate::layout::{MemberLayout, StructLayout};
    use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol, SymbolTable};

    pub const READY_LISTS: u32 = 0x3fca_1000;
    pub const SUSPENDED: u32 = 0x3fca_1100;
    pub const DELAYED_PTR: u32 = 0x3fca_1200;
    pub const DELAYED: u32 = 0x3fca_1300;
    pub const CURRENT: u32 = 0x3fca_1400;
    pub const COUNT: u32 = 0x3fca_1404;
    pub const TICK: u32 = 0x3fca_1408;
    pub const IDLE_HANDLE: u32 = 0x3fca_140c;
    pub const MUTEX: u32 = 0x3fcb_0c40;
    pub const PRIORITIES: u32 = 3;

    /// (address, name, priority, base priority, stack base, free bytes).
    pub const TASKS: [(u32, &str, u32, u32, u32, u32); 3] = [
        (0x3fcb_0000, "main", 22, 1, 0x3fcc_0000, 512),
        (0x3fcb_0200, "esp_timer", 22, 22, 0x3fcc_0400, 900),
        (0x3fcb_0400, "taskLVGL", 4, 4, 0x3fcc_0800, 128),
    ];
    pub const STACK_BYTES: u32 = 1024;

    fn member(path: &str, offset: u32) -> MemberLayout {
        MemberLayout {
            path: path.to_string(),
            offset,
            bits: None,
            size: None,
        }
    }

    /// The FreeRTOS layouts as the official ELF resolves them.
    pub fn layouts() -> Layouts {
        layouts_with_queue_head(true)
    }

    /// Without `QueueDefinition.pcHead`, so no wait can be confirmed to be a mutex.
    pub fn layouts_without_queue_head() -> Layouts {
        layouts_with_queue_head(false)
    }

    fn layouts_with_queue_head(head: bool) -> Layouts {
        let mut l = Layouts::new();
        l.insert(StructLayout::new(
            "xLIST",
            20,
            vec![
                member("uxNumberOfItems", 0),
                member("pxIndex", 4),
                member("xListEnd", 8),
            ],
        ));
        l.insert(StructLayout::new(
            "xLIST_ITEM",
            20,
            vec![
                member("xItemValue", 0),
                member("pxNext", 4),
                member("pxPrevious", 8),
                member("pvOwner", 12),
                member("pxContainer", 16),
            ],
        ));
        l.insert(StructLayout::new(
            "tskTaskControlBlock",
            336,
            vec![
                member("pxTopOfStack", 0),
                member("xStateListItem", 4),
                member("xEventListItem", 24),
                member("uxPriority", 44),
                member("pxStack", 48),
                member("pcTaskName", 52),
                member("pxEndOfStack", 68),
                member("uxBasePriority", 72),
                member("ucNotifyState", 332),
            ],
        ));
        l.insert(StructLayout::new(
            "QueueDefinition",
            84,
            head.then(|| member("pcHead", 0))
                .into_iter()
                .chain([
                    member("u.xSemaphore.xMutexHolder", 8),
                    member("u.xSemaphore.uxRecursiveCallCount", 12),
                    member("xTasksWaitingToSend", 16),
                    member("xTasksWaitingToReceive", 36),
                ])
                .collect(),
        ));
        l
    }

    fn object(name: &str, addr: u32, size: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            addr,
            size,
            kind: SymKind::Object,
            bind: SymBind::Global,
            section: SymSection::Index(1),
        }
    }

    /// The scheduler symbols, with three priorities instead of 25.
    pub fn symbols() -> SymbolTable {
        SymbolTable::new(vec![
            object("pxReadyTasksLists", READY_LISTS, PRIORITIES * 20),
            object("xSuspendedTaskList", SUSPENDED, 20),
            object("pxDelayedTaskList", DELAYED_PTR, 4),
            object("pxCurrentTCBs", CURRENT, 4),
            object("uxCurrentNumberOfTasks", COUNT, 4),
            object("xTickCount", TICK, 4),
            object("xIdleTaskHandle", IDLE_HANDLE, 4),
            object("registered_heaps", 0x3fca_1500, 4),
        ])
    }

    /// An empty `xLIST`: the end marker points at itself.
    pub fn init_list(mem: &mut MemoryImage, at: u32) {
        let end = at + 8;
        mem.put_u32(at, 0);
        mem.put_u32(at + 4, end);
        mem.put_u32(end + 4, end);
        mem.put_u32(end + 8, end);
    }

    pub fn push_item(mem: &mut MemoryImage, at: u32, item: u32, owner: u32) {
        let end = at + 8;
        let count = mem.u32(at).expect("list header");
        let last = mem.u32(end + 8).expect("end pxPrevious");
        mem.put_u32(item + 4, end);
        mem.put_u32(item + 8, last);
        mem.put_u32(item + 12, owner);
        mem.put_u32(item + 16, at);
        mem.put_u32(last + 4, item);
        mem.put_u32(end + 8, item);
        mem.put_u32(at, count + 1);
    }

    pub const MUTEX_B: u32 = 0x3fcb_0d00;
    pub const MUTEX_A: u32 = MUTEX;
    /// Offset of `xTasksWaitingToReceive` in the synthetic `QueueDefinition`.
    pub const RECEIVE: u32 = 36;

    /// Where a task of [`cycle_image`] sits, which says whether its wait can end.
    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub enum Place {
        Ready,
        /// Blocked with a timeout.
        Delayed,
        /// Blocked with no timeout, or suspended outright when its event list is 0.
        Suspended,
    }

    #[derive(Copy, Clone, Debug)]
    pub struct Spec {
        pub tcb: u32,
        pub name: &'static str,
        /// Inheritance raises it above `base`.
        pub prio: u32,
        /// 0 is the idle task.
        pub base: u32,
        pub place: Place,
        /// `xEventListItem.pxContainer`, 0 when it waits on nothing.
        pub event_list: u32,
        pub notify: u8,
    }

    impl Spec {
        pub fn new(tcb: u32, name: &'static str, base: u32, place: Place) -> Spec {
            Spec {
                tcb,
                name,
                prio: base,
                base,
                place,
                event_list: 0,
                notify: 0,
            }
        }

        pub fn waiting_on(mut self, event_list: u32) -> Spec {
            self.event_list = event_list;
            self
        }

        pub fn inherited(mut self, prio: u32) -> Spec {
            self.prio = prio;
            self
        }
    }

    /// A mutex or queue of [`cycle_image`].
    #[derive(Copy, Clone, Debug)]
    pub struct Lock {
        pub at: u32,
        /// `u.xSemaphore.xMutexHolder`, or whatever the union holds in a plain queue.
        pub holder: u32,
        /// 0 in a mutex, the storage in a queue.
        pub head: u32,
    }

    impl Lock {
        pub fn mutex(at: u32, holder: u32) -> Lock {
            Lock {
                at,
                holder,
                head: 0,
            }
        }

        pub fn queue(at: u32, holder: u32) -> Lock {
            Lock {
                at,
                holder,
                head: at + 64,
            }
        }
    }

    /// An image with the given tasks and locks, the first task current and idle; otherwise as
    /// [`image`].
    pub fn cycle_image(specs: &[Spec], locks: &[Lock]) -> MemoryImage {
        let mut mem = MemoryImage::new();
        mem.map_zeroed(READY_LISTS, (PRIORITIES * 20) as usize);
        mem.map_zeroed(SUSPENDED, 20);
        mem.map_zeroed(DELAYED_PTR, 4);
        mem.map_zeroed(DELAYED, 20);
        mem.map_zeroed(CURRENT, 16);
        for p in 0..PRIORITIES {
            init_list(&mut mem, READY_LISTS + p * 20);
        }
        init_list(&mut mem, SUSPENDED);
        init_list(&mut mem, DELAYED);
        mem.put_u32(DELAYED_PTR, DELAYED);
        mem.put_u32(CURRENT, specs.first().map_or(0, |s| s.tcb));
        mem.put_u32(IDLE_HANDLE, specs.first().map_or(0, |s| s.tcb));
        mem.put_u32(COUNT, specs.len() as u32);
        mem.put_u32(TICK, 1234);
        for lock in locks {
            mem.map_zeroed(lock.at, 84);
            mem.put_u32(lock.at, lock.head);
            mem.put_u32(lock.at + 8, lock.holder);
            mem.put_u32(lock.at + 12, 1);
        }
        for (index, spec) in specs.iter().enumerate() {
            let stack = 0x3fcc_0000 + (index as u32) * 0x1000;
            mem.map_zeroed(spec.tcb, 336);
            let mut bytes = vec![STACK_FILL_BYTE; 512];
            bytes.resize(STACK_BYTES as usize, 0x5a);
            mem.map(stack, bytes);
            mem.put(spec.tcb + 52, spec.name.as_bytes());
            mem.put_u32(spec.tcb + 44, spec.prio);
            mem.put_u32(spec.tcb + 72, spec.base);
            mem.put_u32(spec.tcb + 48, stack);
            mem.put_u32(spec.tcb + 68, stack + STACK_BYTES - 1);
            mem.put_u32(spec.tcb, stack + STACK_BYTES - 64);
            mem.put_u32(spec.tcb + 24 + 16, spec.event_list);
            mem.put_u32(spec.tcb + 332, u32::from(spec.notify));
            let list = match spec.place {
                Place::Ready => READY_LISTS + spec.prio.min(PRIORITIES - 1) * 20,
                Place::Delayed => DELAYED,
                Place::Suspended => SUSPENDED,
            };
            push_item(&mut mem, list, spec.tcb + 4, spec.tcb);
        }
        mem
    }

    /// `main` running at an inherited priority of 22, `esp_timer` blocked on the LVGL port mutex
    /// `main` holds, and `taskLVGL` suspended, as on the official firmware after `enter_menu`.
    pub fn image() -> MemoryImage {
        let mut mem = MemoryImage::new();
        mem.map_zeroed(READY_LISTS, (PRIORITIES * 20) as usize);
        mem.map_zeroed(SUSPENDED, 20);
        mem.map_zeroed(DELAYED_PTR, 4);
        mem.map_zeroed(DELAYED, 20);
        mem.map_zeroed(CURRENT, 16);
        mem.map_zeroed(MUTEX, 84);
        for p in 0..PRIORITIES {
            init_list(&mut mem, READY_LISTS + p * 20);
        }
        init_list(&mut mem, SUSPENDED);
        init_list(&mut mem, DELAYED);
        mem.put_u32(DELAYED_PTR, DELAYED);
        mem.put_u32(CURRENT, TASKS[0].0);
        mem.put_u32(COUNT, TASKS.len() as u32);
        mem.put_u32(TICK, 1234);

        for (tcb, name, prio, base, stack, free) in TASKS {
            mem.map_zeroed(tcb, 336);
            let mut bytes = vec![STACK_FILL_BYTE; free as usize];
            bytes.resize(STACK_BYTES as usize, 0x5a);
            mem.map(stack, bytes);
            mem.put(tcb + 52, name.as_bytes());
            mem.put_u32(tcb + 44, prio);
            mem.put_u32(tcb + 72, base);
            mem.put_u32(tcb + 48, stack);
            mem.put_u32(tcb + 68, stack + STACK_BYTES - 1);
            mem.put_u32(tcb, stack + STACK_BYTES - 64);
        }
        push_item(&mut mem, READY_LISTS + 2 * 20, TASKS[0].0 + 4, TASKS[0].0);
        push_item(&mut mem, DELAYED, TASKS[1].0 + 4, TASKS[1].0);
        mem.put_u32(TASKS[1].0 + 24 + 16, MUTEX + 36);
        push_item(&mut mem, SUSPENDED, TASKS[2].0 + 4, TASKS[2].0);
        mem.put_u32(MUTEX + 8, TASKS[0].0);
        mem.put_u32(MUTEX + 12, 1);
        mem
    }
}

#[cfg(test)]
mod tests {
    use super::synthetic::*;
    use super::*;
    use crate::MemoryImage;

    #[test]
    fn the_task_walk_reads_states_priorities_stacks_and_the_lock_holder() {
        let (layouts, syms, mem) = (layouts(), symbols(), image());
        let mut snapshot = walk_tasks(&layouts, &syms, &mem).expect("the walk succeeds");
        assert!(snapshot.is_consistent(), "{:?}", snapshot.warnings);
        assert_eq!(snapshot.tick, Some(1234));
        assert_eq!(snapshot.count, Some(3));
        assert_eq!(snapshot.current, Some(0x3fcb_0000));
        resolve_blocks(
            &mut snapshot,
            &layouts,
            &mem,
            &[("lvgl_port mutex".to_string(), MUTEX)],
        );
        assert_eq!(
            snapshot.render(),
            "main running prio=22(1) stack=512/1024 tcb=0x3fcb0000\n\
             esp_timer blocked prio=22(22) stack=900/1024 tcb=0x3fcb0200 blocked on \
             lvgl_port mutex (waiting to receive)\n\
             taskLVGL suspended prio=4(4) stack=128/1024 tcb=0x3fcb0400\n"
        );
        let main = snapshot.task(0x3fcb_0000).expect("main");
        assert!(main.priority > main.base_priority);
        let lock = MutexInfo::read(&layouts, &mem, MUTEX, Some(&snapshot)).expect("the mutex");
        assert_eq!(lock.holder, Some(0x3fcb_0000));
        assert_eq!(lock.recursive_count, 1);
        assert_eq!(
            lock.render("lvgl_port mutex"),
            "lvgl_port mutex held by main (0x3fcb0000) recursive=1"
        );
    }

    #[test]
    fn a_suspended_task_that_waits_on_an_event_list_is_blocked() {
        let (layouts, syms, mut mem) = (layouts(), symbols(), image());
        mem.put_u32(TASKS[2].0 + 24 + 16, MUTEX + 36);
        let snapshot = walk_tasks(&layouts, &syms, &mem).expect("the walk succeeds");
        let lvgl = snapshot.task(TASKS[2].0).expect("taskLVGL");
        assert_eq!(lvgl.state, TaskState::Blocked);
        let untouched = walk_tasks(&layouts, &syms, &image()).expect("the walk succeeds");
        assert_eq!(
            untouched.task(TASKS[2].0).expect("taskLVGL").state,
            TaskState::Suspended
        );
    }

    #[test]
    fn a_suspended_task_that_waits_on_a_notification_is_blocked() {
        let (layouts, syms, mut mem) = (layouts(), symbols(), image());
        mem.put_u32(TASKS[2].0 + 332, u32::from(TASK_WAITING_NOTIFICATION));
        let snapshot = walk_tasks(&layouts, &syms, &mem).expect("the walk succeeds");
        assert_eq!(
            snapshot.task(TASKS[2].0).expect("taskLVGL").state,
            TaskState::Blocked
        );
        mem.put_u32(TASKS[2].0 + 332, 2);
        let received = walk_tasks(&layouts, &syms, &mem).expect("the walk succeeds");
        assert_eq!(
            received.task(TASKS[2].0).expect("taskLVGL").state,
            TaskState::Suspended,
            "`taskNOTIFICATION_RECEIVED` is not a wait"
        );
    }

    #[test]
    fn a_ready_list_of_unusable_size_is_reported_not_silently_skipped() {
        let (layouts, mem) = (layouts(), image());
        let resized = |size: u32| {
            SymbolTable::new(
                symbols()
                    .iter()
                    .cloned()
                    .map(|mut s| {
                        if s.name == "pxReadyTasksLists" {
                            s.size = size;
                        }
                        s
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let snapshot = walk_tasks(&layouts, &resized(0), &mem).expect("the walk still returns");
        // `main` was in the ready list.
        let names: Vec<&str> = snapshot.tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["esp_timer", "taskLVGL"]);
        assert!(!snapshot.is_consistent());
        let text = snapshot.render();
        assert!(
            text.contains(
                "warning: pxReadyTasksLists at 0x3fca1000: its symbol size 0 holds no \
                 whole xLIST of 20, so no ready list is walked\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "warning: pxCurrentTCBs at 0x3fcb0000: the running task is in none of \
                 the walked lists\n"
            ),
            "{text}"
        );
        let snapshot =
            walk_tasks(&layouts, &resized(2 * 20 + 4), &mem).expect("the walk still returns");
        assert!(
            snapshot.render().contains(
                "warning: pxReadyTasksLists at 0x3fca1000: its symbol size 44 is not a \
                 whole number of xLIST of 20, so only 2 ready lists are walked\n"
            ),
            "{}",
            snapshot.render()
        );
    }

    #[test]
    fn a_list_item_naming_another_container_is_reported_not_followed() {
        let (layouts, syms, mut mem) = (layouts(), symbols(), image());
        mem.put_u32(TASKS[2].0 + 4 + 16, DELAYED);
        let snapshot = walk_tasks(&layouts, &syms, &mem).expect("the walk still returns");
        assert_eq!(snapshot.tasks.len(), 2, "the item is not followed");
        assert!(snapshot.tasks.iter().all(|t| t.name != "taskLVGL"));
        let w = snapshot
            .warnings
            .iter()
            .find(|w| w.at == TASKS[2].0 + 4)
            .expect("the corrupt item is reported");
        assert!(
            w.detail
                .contains("pxContainer is 0x3fca1300, not this list")
        );
        assert!(w.detail.contains("not followed further"));
        assert!(!snapshot.is_consistent());
    }

    #[test]
    fn a_cyclic_list_and_a_wrong_item_count_are_both_reported() {
        let (layouts, syms, mut mem) = (layouts(), symbols(), image());
        // Two items in the ready list, whose links close back on the first.
        push_item(&mut mem, READY_LISTS + 2 * 20, TASKS[2].0 + 4, TASKS[2].0);
        mem.put_u32(TASKS[2].0 + 4 + 4, TASKS[0].0 + 4);
        let snapshot = walk_tasks(&layouts, &syms, &mem).expect("the walk still returns");
        let w = snapshot
            .warnings
            .iter()
            .find(|w| w.detail.contains("cycle") || w.detail.contains("past uxNumberOfItems"))
            .expect("the cycle is reported");
        assert_eq!(w.what, "pxReadyTasksLists");

        let mut mem = image();
        mem.put_u32(SUSPENDED, 4);
        let snapshot = walk_tasks(&layouts, &syms, &mem).expect("the walk still returns");
        let w = snapshot
            .warnings
            .iter()
            .find(|w| w.detail.contains("uxNumberOfItems is 4"))
            .expect("the count mismatch is reported");
        assert_eq!(w.at, SUSPENDED);

        // A count past the cap is refused before any link is read.
        let mut mem = image();
        mem.put_u32(SUSPENDED, MAX_LIST_ITEMS + 1);
        let snapshot = walk_tasks(&layouts, &syms, &mem).expect("the walk still returns");
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|w| w.detail.contains("past the 256 cap"))
        );
    }

    /// TCB of `dl_task_a`, the lowest of the cycle, so the report starts there.
    const DL_A: u32 = 0x3fcb_0000;
    const DL_B: u32 = 0x3fcb_0200;
    /// Neither in the cycle nor idle.
    const OTHER: u32 = 0x3fcb_0400;
    const IDLE: u32 = 0x3fcb_0600;

    fn idle() -> Spec {
        Spec::new(IDLE, "IDLE", 0, Place::Ready)
    }

    /// `probes/probe_deadlock` in miniature: each task holds one mutex and waits forever for the
    /// other.
    fn cycle_specs(place: Place) -> [Spec; 3] {
        [
            idle(),
            Spec::new(DL_A, "dl_task_a", 5, place).waiting_on(MUTEX_B + RECEIVE),
            Spec::new(DL_B, "dl_task_b", 5, place).waiting_on(MUTEX_A + RECEIVE),
        ]
    }

    fn cycle_locks() -> [Lock; 2] {
        [Lock::mutex(MUTEX_A, DL_A), Lock::mutex(MUTEX_B, DL_B)]
    }

    fn resolved(mem: &MemoryImage) -> TaskSnapshot {
        let (layouts, syms) = (layouts(), symbols());
        let mut snapshot = walk_tasks(&layouts, &syms, mem).expect("the walk succeeds");
        resolve_mutex_waits(&mut snapshot, &layouts, mem);
        snapshot
    }

    #[test]
    fn a_two_mutex_cycle_with_nothing_else_to_run_is_a_deadlock() {
        let snapshot = resolved(&cycle_image(&cycle_specs(Place::Suspended), &cycle_locks()));
        assert!(snapshot.is_consistent(), "{:?}", snapshot.warnings);
        let waiter = snapshot.task(DL_A).expect("dl_task_a");
        assert_eq!(waiter.state, TaskState::Blocked);
        assert!(waiter.indefinite, "a portMAX_DELAY wait has no timeout");
        assert_eq!(
            waiter.mutex_wait,
            Some(MutexWait {
                queue: MUTEX_B,
                holder: DL_B,
                recursive_count: 1,
                to_send: false,
            })
        );
        let report = deadlock_report(&snapshot).expect("the cycle is a deadlock");
        assert_eq!(
            report.render(),
            "dl_task_a waits for mutex 0x3fcb0d00 held by dl_task_b\n\
             dl_task_b waits for mutex 0x3fcb0c40 held by dl_task_a\n"
        );
        // The lowest TCB always comes first, whichever task the scan starts from.
        let mut reversed = cycle_specs(Place::Suspended);
        reversed.swap(1, 2);
        let other = resolved(&cycle_image(&reversed, &cycle_locks()));
        assert_eq!(deadlock_report(&other), Some(report));
    }

    /// `probe_deadlock` closes its cycle while `app_main` is still in a `vTaskDelay` with its
    /// own report to print.
    #[test]
    fn a_cycle_is_no_deadlock_while_another_task_can_still_run() {
        let with = |place: Place, base: u32| {
            let specs = cycle_specs(Place::Suspended);
            let mut all = vec![specs[0], specs[1], specs[2]];
            all.push(Spec::new(OTHER, "main", base, place));
            resolved(&cycle_image(&all, &cycle_locks()))
        };
        let delayed = with(Place::Delayed, 1);
        let cycle = mutex_cycle(&delayed).expect("the cycle is still found");
        assert_eq!(cycle.len(), 2);
        assert_eq!(runnable_outside(&delayed, &cycle), ["main"]);
        assert_eq!(deadlock_report(&delayed), None);
        let ready = with(Place::Ready, 1);
        assert_eq!(deadlock_report(&ready), None);
        // Suspended outright: nothing will resume it.
        let suspended = with(Place::Suspended, 1);
        assert!(deadlock_report(&suspended).is_some());
        // Only the task `xIdleTaskHandle` names is exempt: another ready task at the idle
        // priority keeps the guest going.
        assert_eq!(deadlock_report(&with(Place::Ready, 0)), None);
    }

    #[test]
    fn a_livelock_that_is_not_a_cycle_is_never_reported() {
        let specs = [
            idle(),
            Spec::new(DL_A, "dl_task_a", 5, Place::Suspended).waiting_on(MUTEX_B + RECEIVE),
            // `dl_task_b` holds `MUTEX_B` and spins.
            Spec::new(DL_B, "dl_task_b", 5, Place::Ready),
        ];
        let snapshot = resolved(&cycle_image(&specs, &cycle_locks()));
        assert_eq!(mutex_cycle(&snapshot), None, "the chain does not close");
        assert_eq!(deadlock_report(&snapshot), None);
        let waiter = snapshot.task(DL_A).expect("dl_task_a");
        assert_eq!(
            waiter.blocked_on.as_deref(),
            Some("mutex 0x3fcb0d00 held by dl_task_b (waiting to receive, recursive=1)")
        );
    }

    #[test]
    fn a_wait_with_a_timeout_is_never_part_of_a_cycle() {
        let delayed = resolved(&cycle_image(&cycle_specs(Place::Delayed), &cycle_locks()));
        let a = delayed.task(DL_A).expect("dl_task_a");
        assert_eq!(a.state, TaskState::Blocked);
        assert!(!a.indefinite, "a wait in the delayed list has a timeout");
        assert!(a.mutex_wait.is_some(), "the mutex is still named");
        assert_eq!(mutex_cycle(&delayed), None);
        // A timed queue receive whose union word aliases a TCB, and a timed notification
        // wait.
        let specs = [
            idle(),
            Spec::new(DL_A, "dl_task_a", 5, Place::Delayed).waiting_on(MUTEX_B + RECEIVE),
            Spec {
                notify: 1,
                ..Spec::new(DL_B, "dl_task_b", 5, Place::Delayed)
            },
        ];
        let queues = [Lock::mutex(MUTEX_A, DL_A), Lock::queue(MUTEX_B, DL_B)];
        let snapshot = resolved(&cycle_image(&specs, &queues));
        assert_eq!(mutex_cycle(&snapshot), None);
        assert_eq!(deadlock_report(&snapshot), None);
    }

    #[test]
    fn a_priority_inversion_that_resolves_is_not_a_deadlock() {
        let specs = [
            idle(),
            Spec::new(DL_A, "dl_task_a", 9, Place::Suspended).waiting_on(MUTEX_A + RECEIVE),
            Spec::new(DL_B, "dl_task_b", 2, Place::Ready).inherited(9),
        ];
        let snapshot = resolved(&cycle_image(&specs, &[Lock::mutex(MUTEX_A, DL_B)]));
        let holder = snapshot.task(DL_B).expect("dl_task_b");
        assert!(holder.priority > holder.base_priority, "{holder:?}");
        assert_eq!(mutex_cycle(&snapshot), None);
        assert_eq!(deadlock_report(&snapshot), None);
    }

    /// In a plain queue the holder word is `pcReadFrom` and can hold a TCB address. Such a wait is
    /// labelled but never carries a verdict.
    #[test]
    fn a_queue_whose_union_looks_like_a_holder_is_not_a_mutex() {
        let locks = [Lock::mutex(MUTEX_A, DL_A), Lock::queue(MUTEX_B, DL_B)];
        let snapshot = resolved(&cycle_image(&cycle_specs(Place::Suspended), &locks));
        let waiter = snapshot.task(DL_A).expect("dl_task_a");
        assert!(waiter.blocked_on.is_some(), "the wait is still named");
        assert_eq!(waiter.mutex_wait, None, "a queue is no mutex");
        assert_eq!(mutex_cycle(&snapshot), None);
        // A layout with no `pcHead` confirms nothing.
        let layouts = layouts_without_queue_head();
        let mem = cycle_image(&cycle_specs(Place::Suspended), &cycle_locks());
        let mut snapshot = walk_tasks(&layouts, &symbols(), &mem).expect("the walk succeeds");
        resolve_mutex_waits(&mut snapshot, &layouts, &mem);
        assert!(snapshot.task(DL_A).expect("dl_task_a").blocked_on.is_some());
        assert_eq!(mutex_cycle(&snapshot), None);
    }

    #[test]
    fn a_cycle_read_from_a_damaged_walk_is_not_a_verdict() {
        let mut mem = cycle_image(&cycle_specs(Place::Suspended), &cycle_locks());
        mem.put_u32(SUSPENDED, 4);
        let snapshot = resolved(&mem);
        assert!(!snapshot.is_consistent());
        assert!(
            snapshot.task(DL_A).expect("dl_task_a").mutex_wait.is_some(),
            "the tasks were read"
        );
        assert_eq!(mutex_cycle(&snapshot), None);
        assert_eq!(deadlock_report(&snapshot), None);
    }

    #[test]
    fn missing_layouts_and_symbols_are_errors() {
        let mem = image();
        assert_eq!(
            walk_tasks(&Layouts::new(), &symbols(), &mem),
            Err(IntrospectError::MissingStruct { name: "xLIST" })
        );
        let empty = pemu_loader::symbols::SymbolTable::default();
        assert_eq!(
            walk_tasks(&layouts(), &empty, &mem),
            Err(IntrospectError::MissingSymbol {
                name: "pxReadyTasksLists"
            })
        );
    }
}
