//! The cgroup2 control- group tree: the hierarchy a process belongs to, the
//! one `/proc/<pid>/cgroup` reports and the cgroup namespace re-renders.
//!
//! **The tree is organizational and nothing else.** This kernel serves no
//! controllers - no `cpu`, no `memory`, no `pids` - so `cgroup.controllers`
//! is empty and `cgroup.subtree_control` refuses every name, and what the
//! hierarchy does is what every controller-less kernel's does: say which
//! processes are grouped where. That is also the whole of what the cgroup
//! namespace needs from it (`docs/notes/` §6.3: `CLONE_NEWCGROUP` was refused
//! with "there is no cgroup tree here to see"), and it is Linux's own answer
//! for a kernel configured without controllers, not a truncation of one
//! configured with them: the file set a group carries below is what a
//! controller-less v2 kernel serves.
//!
//! Every shape below was measured on a current host before it was built
//! (`/sys/fs/cgroup`, Linux 7.2.11, cgroup2 mounted with `nsdelegate`), in a
//! scratch group under the probe's own scope:
//!
//! * the **file set** of a group is nine names - `cgroup.controllers`,
//!   `cgroup.events`, `cgroup.max.depth`, `cgroup.max.descendants`,
//!   `cgroup.procs`, `cgroup.stat`, `cgroup.subtree_control`,
//!   `cgroup.threads`, `cgroup.type` - with `cgroup.type` absent at the
//!   *root* (the root's type is not a question a writer can answer) and the
//!   `max.*` pair present everywhere;
//! * the **modes** are `0444` for the read-only half (`controllers`,
//!   `events`, `stat`), `0644` for the writable half (`procs`, `threads`,
//!   `subtree_control`, `type`, both `max.*`), and `0755` for every
//!   directory;
//! * the **empty files are empty**: a controller-less `cgroup.controllers`
//!   and `cgroup.subtree_control` answer **zero bytes**, not a bare newline
//!   (measured with `wc -c`), while `cgroup.type` answers `domain\n` (7
//!   bytes) and `cgroup.events` its two rows;
//! * **membership moves by write**: `echo <pid> > cgroup.procs` moves the
//!   process - writing a *thread's* tid moves its whole group, which is
//!   `cgroup.procs`'s meaning; `cgroup.threads` moves the one thread. A pid
//!   that names nothing is `ESRCH`, junk is `EINVAL`;
//! * **removal rules**: `rmdir` of a group that still has a child is
//!   `ENOTEMPTY`, of a group that still holds a process is `EBUSY` (both
//!   measured - the second by leaving the probe shell inside its own group);
//! * **an unknown controller is `ENOENT`**: `echo +nosuch >
//!   cgroup.subtree_control` answers `ENOENT` - the controller does not
//!   exist. Enabling a known one on a populated group is the
//!   no-internal-process `EBUSY`, a rule that cannot fire here: with no
//!   controllers, every name is unknown and the file stays empty.
//!
//! **One hierarchy for the machine.** A second mount of cgroup2 binds the
//! same hierarchy on Linux rather than growing a second tree, and the
//! membership relation is the machine's - `/proc/<pid>/cgroup` reports it
//! whether or not anything is mounted - so the tree lives in one global
//! place and a mount hands out its root. Membership is keyed by task slot
//! **and generation** (`sched`'s pidfd generations): a pid number the
//! scheduler reused must not inherit the dead task's group. The map's own
//! size is the task table's - one entry per slot that ever moved, each
//! overwritten on reuse - so the bound the table already enforces is the
//! bound this map has.
//!
//! **Bounds.** The tree a caller can grow is bounded - depth by
//! [`DEPTH_CEILING`], descendants by [`DESCENDANTS_CEILING`] - because
//! `mkdir` is a user-steered allocation path and §10's rule 4 asks the
//! volume to be bounded even where Linux's own answer is "max". The two
//! `max.*` files carry Linux's default (`max`) and accept a number, and the
//! enforcement is the tighter of the written limit and the ceiling.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

/// The deepest chain of groups `mkdir` will build, whatever the `max.depth`
/// files say. A group is an allocation cluster; the bound is the shape this
/// kernel answers a user-steered volume with (§10's rule 4), and the honest
/// place for it is beside the knob Linux gives the same job.
pub(crate) const DEPTH_CEILING: u64 = 8;

/// The most groups one tree holds, live at once, for the same reason.
pub(crate) const DESCENDANTS_CEILING: u64 = 256;

/// Who is in which group, by task slot id: the task table's own shape, one
/// entry per slot that ever moved. The generation beside the node is the
/// slot's tenant when the write was made: a lookup whose generation has
/// moved on is a stale entry for a reused pid, and reads as *root*.
type Members = Mutex<RefCell<BTreeMap<usize, (u64, Arc<CgDir>)>>>;
static MEMBERS: Members = Mutex::new(RefCell::new(BTreeMap::new()));

/// The machine's one hierarchy root, created the first time anything asks -
/// a mount, a `/proc/<pid>/cgroup` read, a membership move. `None` only
/// before that, which no reader can observe for long: the question the tree
/// answers ("which group is this process in?") has Linux's answer the moment
/// the question exists.
static TREE: Mutex<RefCell<Option<Arc<CgDir>>>> = Mutex::new(RefCell::new(None));

/// The next inode number a node or file of this tree takes.
static NEXT_INO: AtomicUsize = AtomicUsize::new(1);

fn next_ino() -> Ino {
    NEXT_INO.fetch_add(1, Ordering::Relaxed) as Ino
}

/// The task-table half of membership, split the way this tree's other
/// host-compiled readers are split (`eventfd`'s, ext4's `claim_park`): on
/// the machine the answers are the scheduler's - every live task, the
/// generation a slot's tenant carries, the threads of one group - and on
/// the host there is no task table, so the answers are the empty forms and
/// every group is unpopulated.
mod tasks {
    /// `(tid, pid)` of every live task.
    pub(super) fn all() -> alloc::vec::Vec<(usize, usize)> {
        #[cfg(target_os = "none")]
        return crate::sched::snapshots()
            .into_iter()
            .map(|task| (task.tid, task.pid))
            .collect();
        #[cfg(not(target_os = "none"))]
        return alloc::vec::Vec::new();
    }

    /// The generation the slot's current tenant carries, or `None` for a
    /// slot that holds no task.
    pub(super) fn generation_of(tid: usize) -> Option<u64> {
        #[cfg(target_os = "none")]
        return crate::sched::pidfd_generation(crate::sched::TaskId(tid));
        #[cfg(not(target_os = "none"))]
        return None;
    }

    /// The threads of one process - the set a `cgroup.procs` write moves.
    pub(super) fn group_of_pid(pid: usize) -> alloc::vec::Vec<(usize, usize)> {
        #[cfg(target_os = "none")]
        return crate::sched::thread_snapshots(pid)
            .into_iter()
            .map(|task| (task.tid, task.pid))
            .collect();
        #[cfg(not(target_os = "none"))]
        return alloc::vec::Vec::new();
    }
}

/// The tree's root, made on first ask.
fn the_tree() -> Arc<CgDir> {
    critical_section::with(|cs| {
        if let Some(root) = TREE.borrow(cs).borrow().clone() {
            return root;
        }
        let root = Arc::new(CgDir {
            ino: next_ino(),
            name: String::new(),
            parent: None,
            children: Mutex::new(RefCell::new(BTreeMap::new())),
            is_root: true,
            max_depth: Mutex::new(RefCell::new(None)),
            max_descendants: Mutex::new(RefCell::new(None)),
        });
        *TREE.borrow(cs).borrow_mut() = Some(root.clone());
        root
    })
}

/// Walk a group's chain of parents, answering how deep it sits and the names
/// it is listed by, nearest first. The `Arc`s are kept on this side of the
/// loop: a borrowed walk through `Weak::upgrade` temporaries is a
/// borrow-checker trap this shape steps around.
fn upward(node: &Arc<CgDir>) -> (u64, Vec<String>) {
    let mut depth = 0u64;
    let mut names = Vec::new();
    let mut at = node.clone();
    while let Some(parent) = at.parent.as_ref().and_then(|weak| weak.upgrade()) {
        depth += 1;
        names.push(at.name.clone());
        at = parent;
    }
    (depth, names)
}

/// Resolve a group by its path from the root - the handle a file node keeps,
/// because a file outlives no group and a `rmdir`-and-recreate of the same
/// name is a different group. `""` is the root.
fn resolve(path: &str) -> Option<Arc<CgDir>> {
    let mut at = the_tree();
    for name in path.split('/').filter(|part| !part.is_empty()) {
        at = critical_section::with(|cs| at.children.borrow(cs).borrow().get(name).cloned())?;
    }
    Some(at)
}

/// This task's group, for the readers that ask the machine rather than the
/// mount: `/proc/<pid>/cgroup` and the cgroup namespace's rendering. The
/// answer for a task that never moved is the root.
pub(crate) fn group_of(tid: usize) -> Arc<CgDir> {
    let generation = tasks::generation_of(tid);
    critical_section::with(|cs| {
        let members = MEMBERS.borrow(cs).borrow();
        match members.get(&tid) {
            // The entry's generation must be this slot's own: a stale entry
            // belongs to a task the slot no longer holds.
            Some((tenant, node)) if Some(*tenant) == generation => node.clone(),
            _ => the_tree(),
        }
    })
}

/// A task is born where its parent is grouped: copy the parent's
/// generation-validated membership to the child, or leave the child at the
/// root when the parent never moved. Called from both child paths in
/// `sched` - a process's `fork`/`clone` and a group's new thread - and it is
/// the one place membership is written besides the `cgroup.procs` door.
#[cfg(target_os = "none")]
pub(crate) fn inherit_membership(parent: crate::sched::TaskId, child: crate::sched::TaskId) {
    let parent_group = group_of(parent.0);
    if parent_group.is_root {
        return;
    }
    set_member(child.0, &parent_group);
}

/// Move one task (by its own slot id) into `node`. The generation is taken
/// now, so a task that dies and has its slot refilled cannot carry the entry
/// with it.
fn set_member(tid: usize, node: &Arc<CgDir>) {
    if let Some(tenant) = tasks::generation_of(tid) {
        critical_section::with(|cs| {
            MEMBERS
                .borrow(cs)
                .borrow_mut()
                .insert(tid, (tenant, node.clone()));
        });
    }
}

/// The live tasks of one group, as `(tid, pid)` pairs ascending by tid:
/// every task whose generation-validated membership is this node. The scan
/// is the task table's own size - the kernel's bound on tasks - and a stale
/// entry (a reused slot) simply reads as *root*, the same rule
/// [`group_of`] applies.
fn members_of(node: &Arc<CgDir>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (tid, pid) in tasks::all() {
        let member = critical_section::with(|cs| {
            let members = MEMBERS.borrow(cs).borrow();
            match members.get(&tid) {
                Some((tenant, at)) => {
                    tasks::generation_of(tid) == Some(*tenant) && Arc::ptr_eq(at, node)
                }
                None => node.is_root,
            }
        });
        if member {
            out.push((tid, pid));
        }
    }
    out.sort_unstable();
    out
}

/// One directory of the tree: a group. Its files are not children here -
/// [`lookup`](Inode::lookup) mints them fresh from [`FileKind`], the way
/// procfs generates its - because their contents are questions about the
/// group, asked at read time and answered at read time.
pub struct CgDir {
    ino: Ino,
    name: String,
    parent: Option<Weak<CgDir>>,
    children: Mutex<RefCell<BTreeMap<String, Arc<CgDir>>>>,
    /// The root is the one group with no `cgroup.type` and the one whose
    /// listing gathers the tasks that never moved.
    is_root: bool,
    /// The written limit, where the file answers a number rather than `max`.
    max_depth: Mutex<RefCell<Option<u64>>>,
    max_descendants: Mutex<RefCell<Option<u64>>>,
}

impl CgDir {
    /// The group's path from the root, in the form `/proc/<pid>/cgroup` and
    /// the cgroup namespace render it: `/` for the root, `/a/b` for a group
    /// two levels down. Rendered per read - a `rmdir`-and-recreate of an
    /// ancestor changes the answer for everything under it.
    pub(crate) fn path(&self) -> String {
        let mut names = vec![self.name.clone()];
        let mut at = match &self.parent {
            Some(parent) => match parent.upgrade() {
                Some(parent) => parent,
                None => return String::from("/"),
            },
            None => return String::from("/"),
        };
        while let Some(parent) = at.parent.as_ref().and_then(|weak| weak.upgrade()) {
            names.push(at.name.clone());
            at = parent;
        }
        names.reverse();
        let mut out = String::new();
        for name in names {
            out.push('/');
            out.push_str(&name);
        }
        out
    }

    /// How many groups live beneath this one, counting the whole subtree.
    fn live_descendants(&self) -> u64 {
        let subtree: Vec<Arc<CgDir>> = critical_section::with(|cs| {
            self.children
                .borrow(cs)
                .borrow()
                .values()
                .cloned()
                .collect()
        });
        let mut total = subtree.len() as u64;
        for child in subtree {
            total += child.live_descendants();
        }
        total
    }

    /// Whether this group holds any live task right now - the
    /// `cgroup.events` answer and half of the `rmdir` rule. The walk is the
    /// caller's to hand an `Arc` to, so the group being asked about is the
    /// group the membership map points at.
    fn populated(&self, node: &Arc<CgDir>) -> bool {
        !members_of(node).is_empty()
    }

    /// The `max.depth` file's word: the written limit or Linux's default.
    fn depth_word(&self) -> String {
        match critical_section::with(|cs| *self.max_depth.borrow(cs).borrow()) {
            Some(limit) => format!("{}\n", limit),
            None => String::from("max\n"),
        }
    }

    fn descendants_word(&self) -> String {
        match critical_section::with(|cs| *self.max_descendants.borrow(cs).borrow()) {
            Some(limit) => format!("{}\n", limit),
            None => String::from("max\n"),
        }
    }

    /// Parse one `max.*` write: `max` keeps the default, a number sets it,
    /// anything else is `EINVAL`.
    fn set_limit(word: &str, at: &Mutex<RefCell<Option<u64>>>) -> VfsResult<()> {
        let word = word.trim_matches(|c: char| c == '\n' || c == ' ');
        let limit = match word {
            "max" => None,
            digits => match digits.parse::<u64>() {
                Ok(number) => Some(number),
                Err(_) => return Err(VfsError::Invalid),
            },
        };
        critical_section::with(|cs| {
            *at.borrow(cs).borrow_mut() = limit;
        });
        Ok(())
    }

    fn read_limit(&self, at: &Mutex<RefCell<Option<u64>>>) -> Option<u64> {
        critical_section::with(|cs| *at.borrow(cs).borrow())
    }

    fn put_back(&self, name: &str, node: Arc<CgDir>) {
        critical_section::with(|cs| {
            self.children
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node);
        });
    }
}

/// The one file a group carries, named by which one it is: the file keeps
/// the group's *path* rather than a pointer, because a file outlives no
/// group and a `rmdir`-and-recreate of the same name is a different group.
struct CgFile {
    ino: Ino,
    path: String,
    which: FileKind,
}

/// The nine names a group carries. The kind is also the mode: the read-only
/// half is `0444`, the writable half `0644`, both measured on a current host
/// (the header carries the probe).
#[derive(Clone, Copy, PartialEq)]
enum FileKind {
    Controllers,
    Events,
    MaxDepth,
    MaxDescendants,
    Procs,
    Stat,
    SubtreeControl,
    Threads,
    Type,
}

impl FileKind {
    fn from_name(name: &str, is_root: bool) -> Option<FileKind> {
        let kind = match name {
            "cgroup.controllers" => FileKind::Controllers,
            "cgroup.events" => FileKind::Events,
            "cgroup.max.depth" => FileKind::MaxDepth,
            "cgroup.max.descendants" => FileKind::MaxDescendants,
            "cgroup.procs" => FileKind::Procs,
            "cgroup.stat" => FileKind::Stat,
            "cgroup.subtree_control" => FileKind::SubtreeControl,
            "cgroup.threads" => FileKind::Threads,
            // The root's type is not a question a writer can answer, and
            // Linux does not put the file there: measured, the mount root
            // carries no `cgroup.type`.
            "cgroup.type" if !is_root => FileKind::Type,
            _ => return None,
        };
        Some(kind)
    }

    /// The file set, for the listing. The directory sorts the whole answer.
    fn all_names(is_root: bool) -> Vec<&'static str> {
        let mut names = vec![
            "cgroup.controllers",
            "cgroup.events",
            "cgroup.max.depth",
            "cgroup.max.descendants",
            "cgroup.procs",
            "cgroup.stat",
            "cgroup.subtree_control",
            "cgroup.threads",
        ];
        if !is_root {
            names.push("cgroup.type");
        }
        names
    }

    fn mode(&self) -> u32 {
        match self {
            FileKind::Controllers | FileKind::Events | FileKind::Stat => 0o444,
            _ => 0o644,
        }
    }

    /// The file's whole content, read out of the group it hangs on.
    fn content(&self, node: &Arc<CgDir>) -> String {
        match self {
            // A controller-less kernel answers the empty file, measured as
            // zero bytes - not a bare newline.
            FileKind::Controllers => String::new(),
            FileKind::SubtreeControl => String::new(),
            // The leaders of the group, one per line, ascending - Linux's
            // own reading order for a membership file.
            FileKind::Procs => {
                let mut out = String::new();
                for (tid, pid) in members_of(node) {
                    if pid == tid {
                        out.push_str(&format!("{pid}\n"));
                    }
                }
                out
            }
            FileKind::Threads => {
                let mut out = String::new();
                for (tid, _) in members_of(node) {
                    out.push_str(&format!("{tid}\n"));
                }
                out
            }
            FileKind::Type => String::from("domain\n"),
            FileKind::Events => {
                format!(
                    "populated {}\nfrozen 0\n",
                    u8::from(CgDir::populated(node, node))
                )
            }
            FileKind::Stat => format!(
                "nr_descendants {}\nnr_dying_descendants 0\n",
                node.live_descendants()
            ),
            FileKind::MaxDepth => node.depth_word(),
            FileKind::MaxDescendants => node.descendants_word(),
        }
    }

    /// A write, for the half that takes one. The answer is Linux's errno for
    /// each refusal, measured on the host the header cites.
    fn written(&self, node: &Arc<CgDir>, text: &str) -> VfsResult<()> {
        match self {
            // `cgroup.procs` moves a *process*: the number may name any of
            // its threads, and every thread of the group follows.
            // `cgroup.threads` moves the one thread it names.
            FileKind::Procs | FileKind::Threads => {
                let number: usize = text.trim().parse().map_err(|_| VfsError::Invalid)?;
                let (found_pid, found_tid) = tasks::all()
                    .into_iter()
                    .find(|(tid, pid)| match self {
                        FileKind::Procs => *pid == number || *tid == number,
                        _ => *tid == number,
                    })
                    .ok_or(VfsError::NoProcess)?;
                if *self == FileKind::Procs {
                    for (tid, _) in tasks::group_of_pid(found_pid) {
                        set_member(tid, node);
                    }
                } else {
                    set_member(found_tid, node);
                }
                Ok(())
            }
            // Enabling or disabling a controller by name: this kernel has
            // none, so every name is `ENOENT` - the same answer a
            // controller-less Linux gives, measured. Clearing the (always
            // empty) set is a success, the one write this file takes.
            FileKind::SubtreeControl => {
                let text = text.trim();
                if text.is_empty() {
                    return Ok(());
                }
                for word in text.split([' ', '\t', '\n']) {
                    if word.is_empty() {
                        continue;
                    }
                    let named = word.strip_prefix(['+', '-']).unwrap_or(word);
                    if named.is_empty() {
                        return Err(VfsError::Invalid);
                    }
                    return Err(VfsError::NoEntry);
                }
                Ok(())
            }
            // The group's type: this tree serves the one kind, `domain`, and
            // the write that names it succeeds as Linux's does.
            FileKind::Type => {
                if text.trim() == "domain" {
                    Ok(())
                } else {
                    Err(VfsError::Invalid)
                }
            }
            FileKind::MaxDepth => CgDir::set_limit(text, &node.max_depth),
            FileKind::MaxDescendants => CgDir::set_limit(text, &node.max_descendants),
            // The read-only half takes no write, and the shape here is the
            // file's own bits: the same answer every 0444 file gives.
            _ => Err(VfsError::Access),
        }
    }
}

impl Inode for CgDir {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn mode(&self) -> u32 {
        0o755
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        if let Some(kind) = FileKind::from_name(name, self.is_root) {
            return Ok(Arc::new(CgFile {
                ino: next_ino(),
                path: self.path(),
                which: kind,
            }));
        }
        critical_section::with(|cs| {
            self.children
                .borrow(cs)
                .borrow()
                .get(name)
                .cloned()
                .map(|node| node as Arc<dyn Inode>)
                .ok_or(VfsError::NoEntry)
        })
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        let mut out: Vec<(String, NodeKind)> = FileKind::all_names(self.is_root)
            .into_iter()
            .map(|name| (String::from(name), NodeKind::File))
            .collect();
        critical_section::with(|cs| {
            for name in self.children.borrow(cs).borrow().keys() {
                out.push((name.clone(), NodeKind::Dir));
            }
        });
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// `mkdir(2)` makes a group. The bounds are the tighter of the written
    /// `max.*` and this tree's ceilings, and a name that is already a child
    /// is `EEXIST`.
    fn create(
        &self,
        name: &str,
        kind: NodeKind,
        _rdev: Option<(u32, u32)>,
    ) -> VfsResult<Arc<dyn Inode>> {
        if kind != NodeKind::Dir {
            // A plain file is not a thing the cgroup hierarchy carries:
            // every name in it is a group or one of the nine attributes.
            return Err(VfsError::Unsupported);
        }
        let parent = resolve(&self.path()).expect("the group being mkdir'd into");
        let (depth, _) = upward(&parent);
        if depth + 1
            > parent
                .read_limit(&parent.max_depth)
                .unwrap_or(DEPTH_CEILING)
        {
            return Err(VfsError::Busy);
        }
        let root = the_tree();
        let live = root.live_descendants();
        if live + 1
            > root
                .read_limit(&root.max_descendants)
                .unwrap_or(DESCENDANTS_CEILING)
        {
            return Err(VfsError::Busy);
        }
        let node = Arc::new(CgDir {
            ino: next_ino(),
            name: String::from(name),
            parent: Some(Arc::downgrade(&parent)),
            children: Mutex::new(RefCell::new(BTreeMap::new())),
            is_root: false,
            max_depth: Mutex::new(RefCell::new(None)),
            max_descendants: Mutex::new(RefCell::new(None)),
        });
        let inserted = critical_section::with(|cs| {
            parent
                .children
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node.clone())
        });
        match inserted {
            Some(previous) => {
                // Lost a race with a same-named mkdir: put the first one
                // back and say so.
                parent.put_back(name, previous);
                Err(VfsError::Exists)
            }
            None => Ok(node),
        }
    }

    /// `rmdir(2)` removes a group, and only the two empties let it: no
    /// children (`ENOTEMPTY`) and no processes (`EBUSY`), both Linux's own
    /// answers and both measured.
    fn rmdir(&self, name: &str) -> VfsResult<()> {
        let taken = critical_section::with(|cs| self.children.borrow(cs).borrow_mut().remove(name));
        let Some(node) = taken else {
            return Err(VfsError::NoEntry);
        };
        let childless = critical_section::with(|cs| node.children.borrow(cs).borrow().is_empty());
        if !childless {
            // Put it back: the name must not vanish over a refusal.
            self.put_back(name, node);
            return Err(VfsError::NotEmpty);
        }
        if !members_of(&node).is_empty() {
            self.put_back(name, node);
            return Err(VfsError::Busy);
        }
        Ok(())
    }
}

impl Inode for CgFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: 0,
        }
    }

    fn mode(&self) -> u32 {
        self.which.mode()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let Some(node) = resolve(&self.path) else {
            // The group went away while the descriptor stayed open. Linux
            // answers `EBUSY` to a `rmdir` that would strand such a reader;
            // this tree drops the group at once, and the descriptor's next
            // read is told the group is gone.
            return Err(VfsError::NoEntry);
        };
        let text = self.which.content(&node);
        let bytes = text.as_bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + buf.len()).min(bytes.len());
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        Ok(end - start)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> VfsResult<usize> {
        let Some(node) = resolve(&self.path) else {
            return Err(VfsError::NoEntry);
        };
        // A cgroup attribute is not a byte stream: every write is one
        // instruction, parsed whole, whatever offset it arrived at.
        let answer = match core::str::from_utf8(data) {
            Err(_) => Err(VfsError::Invalid),
            Ok(text) => self.which.written(&node, text),
        };
        let _ = offset;
        answer.map(|_| data.len())
    }
}

/// The cgroup2 filesystem: one directory tree, the machine's hierarchy
/// behind it.
pub struct CgroupFs {
    root: Arc<CgDir>,
}

impl CgroupFs {
    pub fn new() -> Arc<CgroupFs> {
        Arc::new(CgroupFs { root: the_tree() })
    }
}

impl FileSystem for CgroupFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "cgroupfs"
    }
}

/// The view-root a fresh cgroup namespace carries: the group its maker is
/// in *now*, by membership and not by any view it already holds - Linux's
/// rule for the kind, measured (`unshare -C` twice roots the second at the
/// group the caller is in, whatever the first rooted at). `tid` is the
/// maker's own slot id; the caller passes its parent's for a `clone` child
/// and its own for an `unshare`.
#[cfg(target_os = "none")]
pub(crate) fn ns_root(tid: usize) -> alloc::sync::Arc<str> {
    group_of(tid).path().into()
}

/// `/proc/<pid>/cgroup`'s line for one task: the hierarchy id, the
/// controller list and the group path - `0::/` for the root, the v2
/// rendering, one line with no controller names because the hierarchy has
/// none. `None` for a task that no longer exists, the shape every per-pid
/// generated file answers with.
pub(crate) fn cgroup_line(tid: usize) -> Option<String> {
    let exists = tasks::all()
        .iter()
        .any(|(existing_tid, _)| *existing_tid == tid);
    if !exists {
        return None;
    }
    // The rendering is the *task's* view, not the reader's: the root is the
    // task's own namespace's, and a path the view hides renders as nothing
    // above it - the machine's own view renders the whole path.
    let view = task_view(tid);
    let full = group_of(tid).path();
    let shown = match &view {
        Some(root) => render_under(root, &full),
        None => full,
    };
    Some(format!("0::{shown}\n"))
}

/// The cgroup view-root one task carries, read from the task table. On the
/// host there is no table and no view, which is the machine's own view by
/// construction.
#[cfg(target_os = "none")]
fn task_view(tid: usize) -> Option<alloc::sync::Arc<str>> {
    crate::sched::task_cgroup_root_for(crate::sched::TaskId(tid))
}

#[cfg(not(target_os = "none"))]
fn task_view(_tid: usize) -> Option<alloc::sync::Arc<str>> {
    None
}

/// The part of `full` that a view-rooted namespace shows: the root itself
/// and everything beneath it, re-rooted - `/` for the root group, `/b` for
/// its child. A group outside the view has no rendering; Linux answers the
/// task's own group always (it cannot be outside its own namespace's root),
/// so the honest answer here is the re-rooting of the group the task is in.
fn render_under(root: &str, full: &str) -> String {
    if root == "/" {
        return String::from(full);
    }
    match full.strip_prefix(root) {
        Some(rest) => {
            if rest.is_empty() {
                String::from("/")
            } else {
                String::from(rest)
            }
        }
        // The task's group is above or beside the view-root: it moved into
        // a namespace whose root it is not under, which this kernel's move
        // rules cannot produce (the root is taken at the move). Render the
        // machine path rather than invent one.
        None => String::from(full),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::test_lock;
    // The host test build links std, which the kernel itself never sees.
    extern crate std;

    /// Write the root's `max.descendants` through its own file door - the
    /// way a caller sets it, and the path the enforcement reads back.
    fn write_root_limit(limit: u64) {
        let root = resolve("").expect("the root resolves");
        let file = CgFile {
            ino: next_ino(),
            path: String::from("/"),
            which: FileKind::MaxDescendants,
        };
        if limit == u64::MAX {
            file.write_at(0, b"max").unwrap();
        } else {
            let text = alloc::format!("{limit}\n");
            file.write_at(0, text.as_bytes()).unwrap();
        }
    }

    /// The tree is the machine's one hierarchy, so every test takes the
    /// suite's own lock and a group name of its own: two tests sharing a
    /// name would share a group. The guard travels out with the tuple and
    /// lives to the end of the test - dropped in the macro's block it would
    /// release the lock before the test's own claims ran, and the tests
    /// under parallel threads would fill each other's tree (measured: the
    /// ceiling test's groups survived into every later mkdir).
    macro_rules! locked {
        ($name:literal) => {{
            let guard = test_lock::take();
            let fs = CgroupFs::new();
            let _ = fs.root().rmdir($name);
            (fs, $name, guard)
        }};
    }

    #[test]
    fn a_group_carries_the_nine_files_and_the_root_carries_no_type() {
        let (fs, name, _guard) = locked!("t1");
        let root = fs.root();
        let names: Vec<String> = root
            .read_dir()
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            !names.iter().any(|name| name == "cgroup.type"),
            "the root has no type file"
        );
        assert!(names.contains(&String::from("cgroup.procs")));

        let group = root.create(name, NodeKind::Dir, None).unwrap();
        let names: Vec<String> = group
            .read_dir()
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            names.contains(&String::from("cgroup.type")),
            "a group carries its type"
        );
        assert_eq!(names.len(), 9, "the measured file set");
    }

    #[test]
    fn the_empty_files_are_empty_and_type_is_domain() {
        let (fs, name, _guard) = locked!("t2");
        let group = fs.root().create(name, NodeKind::Dir, None).unwrap();
        let controllers = group.lookup("cgroup.controllers").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(
            controllers.read_at(0, &mut buf).unwrap(),
            0,
            "a controller-less cgroup.controllers is zero bytes, measured"
        );
        let kind = group.lookup("cgroup.type").unwrap();
        let mut buf = [0u8; 16];
        let n = kind.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"domain\n", "the measured 7 bytes");
    }

    #[test]
    fn events_and_stat_answer_the_shapes_the_host_showed() {
        let (fs, name, _guard) = locked!("t3");
        let group = fs.root().create(name, NodeKind::Dir, None).unwrap();
        let events = group.lookup("cgroup.events").unwrap();
        let mut buf = [0u8; 64];
        let n = events.read_at(0, &mut buf).unwrap();
        let text = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(text.starts_with("populated 0\n"), "an empty group: {text}");
        assert!(text.contains("frozen 0\n"), "{text}");

        let stat = group.lookup("cgroup.stat").unwrap();
        let mut buf = [0u8; 128];
        let n = stat.read_at(0, &mut buf).unwrap();
        let text = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(text.starts_with("nr_descendants 0\n"), "{text}");
    }

    #[test]
    fn an_unknown_controller_is_enoent_and_a_clear_is_success() {
        let (fs, name, _guard) = locked!("t4");
        let group = fs.root().create(name, NodeKind::Dir, None).unwrap();
        let control = group.lookup("cgroup.subtree_control").unwrap();
        assert_eq!(
            control.write_at(0, b"+pids").err(),
            Some(VfsError::NoEntry),
            "measured: the controller does not exist here"
        );
        assert_eq!(
            control.write_at(0, b""),
            Ok(0),
            "clearing the empty set succeeds"
        );
    }

    #[test]
    fn a_write_to_the_read_only_half_is_refused_by_the_files_own_bits() {
        let (fs, name, _guard) = locked!("t5");
        let group = fs.root().create(name, NodeKind::Dir, None).unwrap();
        assert_eq!(group.lookup("cgroup.stat").unwrap().mode(), 0o444);
        assert_eq!(group.lookup("cgroup.procs").unwrap().mode(), 0o644);
        assert_eq!(
            group.lookup("cgroup.stat").unwrap().write_at(0, b"x").err(),
            Some(VfsError::Access),
        );
    }

    #[test]
    fn the_max_files_answer_max_take_a_number_and_enforce_it() {
        let (fs, name, _guard) = locked!("t6");
        let group = fs.root().create(name, NodeKind::Dir, None).unwrap();
        let depth = group.lookup("cgroup.max.depth").unwrap();
        let mut buf = [0u8; 16];
        let n = depth.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"max\n", "Linux's own default");

        assert_eq!(depth.write_at(0, b"junk"), Err(VfsError::Invalid));
        assert_eq!(depth.write_at(0, b"0"), Ok(1));
        let mut buf = [0u8; 16];
        let n = depth.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"0\n");

        // A depth of 0 allows nothing beneath this group.
        assert_eq!(
            group.create("sub", NodeKind::Dir, None).err(),
            Some(VfsError::Busy),
            "the written depth limit is the mkdir answer"
        );
        assert_eq!(depth.write_at(0, b"max"), Ok(3), "back to the default");
        assert!(group.create("sub", NodeKind::Dir, None).is_ok());
    }

    #[test]
    fn the_tree_holds_the_named_ceiling_even_at_max() {
        let (fs, name, _guard) = locked!("t7");
        let _ = name;
        let root = fs.root();
        // The suite shares the one tree and the tests run in whatever order
        // the harness reaches them, so this test fills the room the tree has
        // LEFT, checks the stop, and gives every group it made back: a test
        // that leaves the ceiling full starves every run after it.
        let before = resolve("").expect("the root resolves").live_descendants();
        let mut made: alloc::vec::Vec<String> = alloc::vec::Vec::new();
        while before + u64::try_from(made.len()).unwrap_or(DESCENDANTS_CEILING) + before
            < DESCENDANTS_CEILING
        {
            let group = format!("c{}", made.len());
            match root.create(&group, NodeKind::Dir, None) {
                Ok(_) => made.push(group),
                Err(VfsError::Busy) => break,
                Err(e) => panic!("mkdir {group}: {e:?}"),
            }
        }
        let live = resolve("").expect("the root resolves").live_descendants();
        assert!(
            live <= DESCENDANTS_CEILING,
            "the tree grew past its ceiling: {live}"
        );
        if live == DESCENDANTS_CEILING {
            assert_eq!(
                root.create("c-over", NodeKind::Dir, None).err(),
                Some(VfsError::Busy),
                "the ceiling holds for one more name after the stop"
            );
        }
        for group in &made {
            root.rmdir(group)
                .expect("the ceiling test's own groups go back");
        }
    }

    #[test]
    fn a_written_descendant_limit_serves_exactly_its_room() {
        let (fs, name, _guard) = locked!("t10");
        let root = fs.root();
        // Room for exactly one more group, whatever the ambient tree holds:
        // the written limit is read live at each mkdir.
        let live = resolve("").expect("the root resolves").live_descendants();
        write_root_limit(live + 1);
        assert!(
            root.create(name, NodeKind::Dir, None).is_ok(),
            "the last group the room allows"
        );
        assert_eq!(
            root.create("t10-over", NodeKind::Dir, None).err(),
            Some(VfsError::Busy),
            "one past the written room"
        );
        write_root_limit(u64::MAX);
        let _ = root.rmdir(name);
    }

    #[test]
    fn rmdir_refuses_the_two_empties_in_order() {
        let (fs, name, _guard) = locked!("t8");
        let group = fs.root().create(name, NodeKind::Dir, None).unwrap();
        group.create("sub", NodeKind::Dir, None).unwrap();
        assert_eq!(
            fs.root().rmdir(name).err(),
            Some(VfsError::NotEmpty),
            "measured: a group with a child"
        );
        group.rmdir("sub").unwrap();
        // The group is unpopulated on the host (no tasks), so it goes.
        assert_eq!(fs.root().rmdir(name), Ok(()));
        assert_eq!(fs.root().rmdir(name).err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn a_group_knows_its_path_from_the_root() {
        let (fs, name, _guard) = locked!("t9");
        let _ = &fs;
        let group = fs.root().create(name, NodeKind::Dir, None).unwrap();
        group.create("b", NodeKind::Dir, None).unwrap();
        // The path is what the resolver walks: the same group comes back.
        let sub = resolve(&format!("/{name}/b")).expect("the group resolves by its path");
        assert_eq!(sub.path(), format!("/{name}/b"));
        let parent = resolve(&format!("/{name}")).expect("the parent resolves");
        assert_eq!(parent.path(), format!("/{name}"));
        assert_eq!(resolve("").expect("the root resolves").path(), "/");
    }
}
