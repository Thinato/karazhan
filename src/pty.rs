//! Daemon-owned PTY sessions for the embedded terminal pane.
//!
//! [`PtyHost`] owns up to [`MAX_SESSIONS`] `$SHELL` sessions per worktree.
//! Each session's task feeds PTY output into a vt100 parser (scrollback 0);
//! viewers never see raw bytes, only `state_formatted`/`state_diff` frames of
//! that screen, gated to one per [`FRAME`] and coalesced through a `watch`.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::{tcgetpgrp, Pid};
use tokio::io::unix::AsyncFd;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;

use crate::ipc::{SupervisorMsg, TermCmd};

pub const MAX_SESSIONS: u8 = 9;
/// Minimum interval between two frames to one viewer (~60 fps).
pub const FRAME: Duration = Duration::from_millis(16);
/// Cap on input queued in a session task while the PTY is not writable.
const MAX_PENDING_INPUT: usize = 64 * 1024;

/// The lowest slot in `1..=MAX_SESSIONS` not in `used`.
pub fn lowest_free_slot(used: &[u8]) -> Option<u8> {
    (1..=MAX_SESSIONS).find(|s| !used.contains(s))
}

/// Counts BEL characters seen by the parser.
#[derive(Default)]
pub struct BellCount(u64);

impl vt100::Callbacks for BellCount {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.0 += 1;
    }
}

type Key = (PathBuf, u8);

#[derive(Clone)]
pub struct PtyHost {
    sessions: Arc<Mutex<BTreeMap<Key, Arc<Session>>>>,
    events: broadcast::Sender<SupervisorMsg>,
    shell: PathBuf,
}

pub struct Session {
    path: PathBuf,
    slot: u8,
    pid: u32,
    input_tx: mpsc::Sender<Vec<u8>>,
    kill_tx: mpsc::Sender<()>,
    /// Bumped after every screen change; set to `true` once the session ended.
    frames: watch::Sender<bool>,
    state: Mutex<State>,
    #[cfg(test)]
    loops: std::sync::atomic::AtomicUsize,
}

struct State {
    parser: vt100::Parser<BellCount>,
    /// Viewing connection id → its pane size.
    viewers: HashMap<u64, (u16, u16)>,
    /// The connection whose size the PTY follows (last to send input).
    owner: Option<u64>,
    attention: bool,
    /// The master fd while the session task holds it open (for `TIOCSWINSZ`).
    master: Option<RawFd>,
}

/// Per-connection terminal state.
pub struct Conn {
    id: u64,
    view: Option<View>,
}

struct View {
    session: Arc<Session>,
    frames: watch::Receiver<bool>,
    last_sent: Option<vt100::Screen>,
    next_at: Instant,
}

impl Default for Conn {
    fn default() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            view: None,
        }
    }
}

impl Conn {
    /// The next frame of the attached session.  Pends forever while detached;
    /// cancel-safe.
    pub async fn next_frame(&mut self) -> SupervisorMsg {
        loop {
            let Some(view) = self.view.as_mut() else {
                return std::future::pending().await;
            };
            tokio::time::sleep_until(view.next_at).await;
            let alive = view.frames.changed().await.is_ok() && !*view.frames.borrow();
            // A session that ended before its first frame still sends one
            // screen, so a pending create gets its reply.
            if alive || view.last_sent.is_none() {
                let frame = view.frame();
                if !alive {
                    self.view = None;
                }
                return frame;
            }
            self.view = None;
        }
    }
}

impl View {
    /// A full screen on the first frame or after a resize, else a diff.
    fn frame(&mut self) -> SupervisorMsg {
        let cur = lock(&self.session.state).parser.screen().clone();
        let worktree_path = self.session.path.clone();
        let slot = self.session.slot;
        let msg = match &self.last_sent {
            Some(prev) if prev.size() == cur.size() => SupervisorMsg::TermDiff {
                worktree_path,
                slot,
                bytes: cur.state_diff(prev),
            },
            _ => {
                let (rows, cols) = cur.size();
                SupervisorMsg::TermScreen {
                    worktree_path,
                    slot,
                    rows,
                    cols,
                    bytes: cur.state_formatted(),
                }
            }
        };
        self.last_sent = Some(cur);
        self.next_at = Instant::now() + FRAME;
        msg
    }
}

fn term_error(worktree_path: PathBuf, message: impl Into<String>) -> SupervisorMsg {
    SupervisorMsg::TermError {
        worktree_path,
        message: message.into(),
    }
}

/// Lock ignoring poison: a panic elsewhere must not take every session down.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `f` on the parser, still under the session lock.  If vt100 panics,
/// replace the parser with a blank `size` one so the screen stays usable.
/// Returns whether `f` completed.
fn guard_parser(
    st: &mut State,
    size: (u16, u16),
    f: impl FnOnce(&mut vt100::Parser<BellCount>),
) -> bool {
    if catch_unwind(AssertUnwindSafe(|| f(&mut st.parser))).is_ok() {
        return true;
    }
    tracing::warn!("pty: vt100 panicked; resetting the session screen");
    st.parser = vt100::Parser::new_with_callbacks(size.0, size.1, 0, BellCount::default());
    false
}

/// Clamp a client-supplied size so a bogus value cannot allocate a huge grid.
/// The floor is 2×2: vt100 underflows on a 1-row or 1-column grid (and the
/// panic would poison the session lock).
fn clamp_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.clamp(2, 500), cols.clamp(2, 1000))
}

fn key_range(path: &Path) -> std::ops::RangeInclusive<Key> {
    (path.to_path_buf(), 0)..=(path.to_path_buf(), u8::MAX)
}

/// Resize the PTY to connection `id`'s size if it may drive it: it is the
/// owner, or the owner is not viewing.  Returns whether the size changed.
fn apply_size(st: &mut State, id: u64) -> bool {
    let may = st.owner == Some(id) || !st.owner.is_some_and(|o| st.viewers.contains_key(&o));
    let Some(&(rows, cols)) = st.viewers.get(&id) else {
        return false;
    };
    if !may || st.parser.screen().size() == (rows, cols) {
        return false;
    }
    if let Some(fd) = st.master {
        let ws = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `fd` is open while `master` is Some (cleared under this lock
        // before the session task closes it); TIOCSWINSZ reads a winsize.
        unsafe { nix::libc::ioctl(fd, nix::libc::TIOCSWINSZ as _, &ws) };
    }
    guard_parser(st, (rows, cols), |p| p.screen_mut().set_size(rows, cols));
    true
}

impl PtyHost {
    pub fn new(events: broadcast::Sender<SupervisorMsg>) -> Self {
        let shell = std::env::var_os("SHELL").map_or_else(|| "/bin/sh".into(), PathBuf::from);
        Self {
            sessions: Arc::default(),
            events,
            shell,
        }
    }

    /// Apply one terminal command for `conn`; returns a reply for that
    /// connection only (never broadcast).
    pub fn handle(&self, conn: &mut Conn, cmd: TermCmd) -> Option<SupervisorMsg> {
        match cmd {
            TermCmd::Attach {
                worktree_path,
                slot,
                rows,
                cols,
            } => self.attach(conn, worktree_path, slot, rows, cols),
            TermCmd::Detach => {
                self.detach(conn);
                None
            }
            TermCmd::Input(bytes) => {
                let session = &conn.view.as_ref()?.session;
                let resized = {
                    let mut st = lock(&session.state);
                    st.owner = Some(conn.id);
                    apply_size(&mut st, conn.id)
                };
                if resized {
                    session.frames.send_modify(|_| {});
                }
                match session.input_tx.try_send(bytes) {
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        Some(term_error(session.path.clone(), "shell not reading"))
                    }
                    _ => None,
                }
            }
            TermCmd::Resize { rows, cols } => {
                let session = &conn.view.as_ref()?.session;
                let resized = {
                    let mut st = lock(&session.state);
                    st.viewers.insert(conn.id, clamp_size(rows, cols));
                    apply_size(&mut st, conn.id)
                };
                if resized {
                    session.frames.send_modify(|_| {});
                }
                None
            }
            TermCmd::Kill {
                worktree_path,
                slot,
            } => {
                let removed = lock(&self.sessions).remove(&(worktree_path.clone(), slot));
                let Some(session) = removed else {
                    return Some(term_error(worktree_path, format!("no session {slot}")));
                };
                self.broadcast_sessions(&worktree_path);
                let _ = session.kill_tx.try_send(());
                None
            }
        }
    }

    fn attach(
        &self,
        conn: &mut Conn,
        path: PathBuf,
        slot: Option<u8>,
        rows: u16,
        cols: u16,
    ) -> Option<SupervisorMsg> {
        let (rows, cols) = clamp_size(rows, cols);
        let session = match slot {
            Some(n) => {
                let found = lock(&self.sessions).get(&(path.clone(), n)).cloned();
                match found {
                    Some(s) => s,
                    None => return Some(term_error(path, format!("no session {n}"))),
                }
            }
            None => match self.spawn(&path, rows, cols) {
                Ok(s) => s,
                Err(e) => return Some(term_error(path, e)),
            },
        };
        self.detach(conn);
        let (resized, cleared) = {
            let mut st = lock(&session.state);
            st.viewers.insert(conn.id, (rows, cols));
            st.owner.get_or_insert(conn.id);
            (
                apply_size(&mut st, conn.id),
                std::mem::take(&mut st.attention),
            )
        };
        if resized {
            session.frames.send_modify(|_| {});
        }
        if slot.is_none() || cleared {
            self.broadcast_sessions(&path);
        }
        let mut frames = session.frames.subscribe();
        frames.mark_changed();
        conn.view = Some(View {
            session,
            frames,
            last_sent: None,
            next_at: Instant::now(),
        });
        None
    }

    /// Stop viewing; call on every connection exit.
    pub fn detach(&self, conn: &mut Conn) {
        if let Some(view) = conn.view.take() {
            lock(&view.session.state).viewers.remove(&conn.id);
        }
    }

    /// Kill every session of `path`.  Never waits: the slots free immediately
    /// and each session task reaps its shell in the background.
    pub fn kill_worktree(&self, path: &Path) {
        let mut killed = Vec::new();
        lock(&self.sessions).retain(|(p, _), s| {
            if p == path {
                killed.push(Arc::clone(s));
            }
            p != path
        });
        if killed.is_empty() {
            return;
        }
        self.broadcast_sessions(path);
        for s in killed {
            let _ = s.kill_tx.try_send(());
        }
    }

    /// One `TermSessions` per worktree with live sessions (connect / lag resync).
    pub fn sessions_msgs(&self) -> Vec<SupervisorMsg> {
        let mut paths: Vec<PathBuf> = lock(&self.sessions)
            .keys()
            .map(|(p, _)| p.clone())
            .collect();
        paths.dedup();
        paths.iter().map(|p| self.sessions_msg(p)).collect()
    }

    fn sessions_msg(&self, path: &Path) -> SupervisorMsg {
        let map = lock(&self.sessions);
        let mut slots = Vec::new();
        let mut attention = false;
        for ((_, slot), s) in map.range(key_range(path)) {
            slots.push(*slot);
            attention |= lock(&s.state).attention;
        }
        SupervisorMsg::TermSessions {
            worktree_path: path.to_path_buf(),
            slots,
            attention,
        }
    }

    fn broadcast_sessions(&self, path: &Path) {
        let _ = self.events.send(self.sessions_msg(path));
    }

    /// Spawn `$SHELL` in `path` in the lowest free slot.
    fn spawn(&self, path: &Path, rows: u16, cols: u16) -> Result<Arc<Session>, String> {
        let mut map = lock(&self.sessions);
        let used: Vec<u8> = map.range(key_range(path)).map(|((_, s), _)| *s).collect();
        let slot = lowest_free_slot(&used).ok_or("max 9 sessions")?;
        let session = self
            .spawn_session(path, slot, rows, cols)
            .map_err(|e| format!("failed to start shell: {e}"))?;
        map.insert((path.to_path_buf(), slot), Arc::clone(&session));
        Ok(session)
    }

    fn spawn_session(
        &self,
        path: &Path,
        slot: u8,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Arc<Session>> {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};

        let ws = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(&ws, None)?;
        for fd in [&pty.master, &pty.slave] {
            fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        }
        fcntl(pty.master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;

        let mut cmd = tokio::process::Command::new(&self.shell);
        cmd.current_dir(path)
            .env("TERM", "xterm-256color")
            .stdin(pty.slave.try_clone()?)
            .stdout(pty.slave.try_clone()?)
            .stderr(pty.slave)
            .kill_on_drop(true);
        // SAFETY: only async-signal-safe calls (setsid, ioctl) between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // Close our copies of the slave so EOF arrives when the shell exits.
        drop(cmd);
        let master = AsyncFd::new(std::fs::File::from(pty.master))?;

        let (input_tx, input_rx) = mpsc::channel(64);
        let (kill_tx, kill_rx) = mpsc::channel(1);
        let session = Arc::new(Session {
            path: path.to_path_buf(),
            slot,
            pid: child.id().unwrap_or_default(),
            input_tx,
            kill_tx,
            frames: watch::Sender::new(false),
            state: Mutex::new(State {
                parser: vt100::Parser::new_with_callbacks(rows, cols, 0, BellCount::default()),
                viewers: HashMap::new(),
                owner: None,
                attention: false,
                master: Some(master.as_raw_fd()),
            }),
            #[cfg(test)]
            loops: Default::default(),
        });
        tokio::spawn(run_session(
            self.clone(),
            Arc::clone(&session),
            child,
            master,
            input_rx,
            kill_rx,
        ));
        Ok(session)
    }

    /// Remove `session` from the map if it still occupies its slot.
    fn remove_if_same(&self, session: &Arc<Session>) {
        let key = (session.path.clone(), session.slot);
        let removed = {
            let mut map = lock(&self.sessions);
            let same = map.get(&key).is_some_and(|s| Arc::ptr_eq(s, session));
            same && map.remove(&key).is_some()
        };
        if removed {
            self.broadcast_sessions(&session.path);
        }
    }
}

impl Session {
    /// Feed PTY output to the parser; broadcasts on the unwatched-bell edge.
    fn process(&self, host: &PtyHost, bytes: &[u8]) {
        let edge = {
            let mut st = lock(&self.state);
            let before = st.parser.callbacks().0;
            let size = st.parser.screen().size();
            let ok = guard_parser(&mut st, size, |p| p.process(bytes));
            let rang = ok && st.parser.callbacks().0 != before;
            let edge = rang && st.viewers.is_empty() && !st.attention;
            st.attention |= edge;
            edge
        };
        self.frames.send_modify(|_| {});
        if edge {
            host.broadcast_sessions(&self.path);
        }
    }
}

async fn run_session(
    host: PtyHost,
    session: Arc<Session>,
    mut child: tokio::process::Child,
    master: AsyncFd<std::fs::File>,
    mut input_rx: mpsc::Receiver<Vec<u8>>,
    mut kill_rx: mpsc::Receiver<()>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut pending: Vec<u8> = Vec::new();
    // EOF/EIO (hangup): a hung-up fd stays readable forever, so stop polling it.
    let mut read_done = false;
    let killed = loop {
        #[cfg(test)]
        session.loops.fetch_add(1, Ordering::Relaxed);
        tokio::select! {
            r = master.readable(), if !read_done => {
                let Ok(mut guard) = r else {
                    read_done = true;
                    continue;
                };
                match guard.try_io(|f| f.get_ref().read(&mut buf)) {
                    Ok(Ok(0)) | Ok(Err(_)) => read_done = true,
                    Ok(Ok(n)) => session.process(&host, &buf[..n]),
                    Err(_would_block) => {}
                }
            }
            w = master.writable(), if !pending.is_empty() && !read_done => {
                let Ok(mut guard) = w else {
                    pending.clear();
                    continue;
                };
                match guard.try_io(|f| f.get_ref().write(&pending)) {
                    Ok(Ok(n)) => {
                        pending.drain(..n);
                    }
                    Ok(Err(_)) => pending.clear(),
                    Err(_would_block) => {}
                }
            }
            Some(bytes) = input_rx.recv(), if pending.len() < MAX_PENDING_INPUT => {
                pending.extend_from_slice(&bytes);
            }
            _ = child.wait() => break false,
            _ = kill_rx.recv() => break true,
        }
    };

    // The foreground job, read while the master is still open.
    let fg = tcgetpgrp(master.get_ref()).ok();
    if killed {
        // SIGHUP the foreground job and the shell's own group (the shell is a
        // session leader, so its pgid is its pid).
        if let Some(fg) = fg {
            let _ = killpg(fg, Signal::SIGHUP);
        }
        let _ = killpg(Pid::from_raw(session.pid as i32), Signal::SIGHUP);
    }
    lock(&session.state).master = None;
    // Closing the master hangs up the terminal.
    drop(master);
    if killed
        && tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
    {
        if let Some(fg) = fg {
            let _ = killpg(fg, Signal::SIGKILL);
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    host.remove_if_same(&session);
    session.frames.send_replace(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn host() -> (PtyHost, broadcast::Receiver<SupervisorMsg>) {
        let (tx, rx) = broadcast::channel(256);
        let mut host = PtyHost::new(tx);
        host.shell = "/bin/sh".into();
        (host, rx)
    }

    /// A fake TUI connection: a `Conn` plus the client-side mirror parser.
    struct Client {
        conn: Conn,
        term: Option<vt100::Parser>,
        slot: Option<u8>,
        frames: usize,
    }

    impl Client {
        fn new() -> Self {
            Self {
                conn: Conn::default(),
                term: None,
                slot: None,
                frames: 0,
            }
        }

        /// Attach (`slot: None` creates) and wait for the first full screen.
        async fn attach(host: &PtyHost, path: &Path, slot: Option<u8>, size: (u16, u16)) -> Self {
            let mut c = Self::new();
            c.attach_again(host, path, slot, size).await;
            c
        }

        async fn attach_again(
            &mut self,
            host: &PtyHost,
            path: &Path,
            slot: Option<u8>,
            (rows, cols): (u16, u16),
        ) {
            let reply = self.send(
                host,
                TermCmd::Attach {
                    worktree_path: path.to_path_buf(),
                    slot,
                    rows,
                    cols,
                },
            );
            assert!(reply.is_none(), "attach failed: {reply:?}");
            let first = self.frame(WAIT).await.expect("first frame");
            assert!(
                matches!(first, SupervisorMsg::TermScreen { .. }),
                "first frame not a screen"
            );
        }

        fn send(&mut self, host: &PtyHost, cmd: TermCmd) -> Option<SupervisorMsg> {
            host.handle(&mut self.conn, cmd)
        }

        fn input(&mut self, host: &PtyHost, s: &str) {
            assert!(self
                .send(host, TermCmd::Input(s.as_bytes().to_vec()))
                .is_none());
        }

        /// Receive one frame into the mirror parser.
        async fn frame(&mut self, wait: Duration) -> Option<SupervisorMsg> {
            let msg = tokio::time::timeout(wait, self.conn.next_frame())
                .await
                .ok()?;
            self.frames += 1;
            match &msg {
                SupervisorMsg::TermScreen {
                    slot,
                    rows,
                    cols,
                    bytes,
                    ..
                } => {
                    let mut p = vt100::Parser::new(*rows, *cols, 0);
                    p.process(bytes);
                    self.term = Some(p);
                    self.slot = Some(*slot);
                }
                SupervisorMsg::TermDiff { bytes, .. } => self
                    .term
                    .as_mut()
                    .expect("diff before screen")
                    .process(bytes),
                other => panic!("unexpected frame {other:?}"),
            }
            Some(msg)
        }

        fn session(&self) -> Arc<Session> {
            Arc::clone(&self.conn.view.as_ref().expect("attached").session)
        }

        fn daemon_screen(&self) -> vt100::Screen {
            self.session().state.lock().unwrap().parser.screen().clone()
        }

        fn contents(&self) -> String {
            self.term
                .as_ref()
                .map(|t| t.screen().contents())
                .unwrap_or_default()
        }

        async fn until(&mut self, needle: &str) {
            let deadline = Instant::now() + WAIT;
            while !self.contents().contains(needle) {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    self.frame(left).await.is_some(),
                    "timed out waiting for {needle:?}; screen:\n{}",
                    self.contents()
                );
            }
        }

        /// Pump frames until the mirror matches the daemon's screen.
        async fn converge(&mut self) {
            let deadline = Instant::now() + WAIT;
            loop {
                let d = self.daemon_screen();
                let t = self.term.as_ref().expect("screen").screen();
                if t.size() == d.size()
                    && t.contents() == d.contents()
                    && t.cursor_position() == d.cursor_position()
                {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "mirror diverged\nmirror:\n{}\ndaemon:\n{}",
                    t.contents(),
                    d.contents()
                );
                self.frame(Duration::from_millis(200)).await;
            }
        }
    }

    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn next_sessions(rx: &mut broadcast::Receiver<SupervisorMsg>) -> (Vec<u8>, bool) {
        loop {
            match tokio::time::timeout(WAIT, rx.recv())
                .await
                .expect("TermSessions")
            {
                Ok(SupervisorMsg::TermSessions {
                    slots, attention, ..
                }) => return (slots, attention),
                Ok(_) => {}
                Err(e) => panic!("broadcast: {e}"),
            }
        }
    }

    fn slots(host: &PtyHost, path: &Path) -> Vec<u8> {
        match host.sessions_msg(path) {
            SupervisorMsg::TermSessions { slots, .. } => slots,
            _ => unreachable!(),
        }
    }

    fn ended(s: &Session) -> bool {
        *s.frames.borrow()
    }

    fn is_alive(pid: Pid) -> bool {
        nix::sys::signal::kill(pid, None).is_ok()
    }

    #[test]
    fn vt100_panic_resets_parser_without_poisoning() {
        // A 1x1 grid (below the clamp floor) makes vt100 underflow on a wide char.
        let st = Mutex::new(State {
            parser: vt100::Parser::new_with_callbacks(1, 1, 0, BellCount::default()),
            viewers: HashMap::new(),
            owner: None,
            attention: false,
            master: None,
        });
        {
            let mut g = lock(&st);
            assert!(!guard_parser(&mut g, (3, 4), |p| p.process("漢漢".as_bytes())));
            assert_eq!(g.parser.screen().size(), (3, 4));
        }
        assert!(!st.is_poisoned());
        // A lock poisoned elsewhere is still usable.
        let m = Mutex::new(1);
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _g = m.lock().unwrap();
            panic!("poison");
        }));
        assert_eq!(*lock(&m), 1);
    }

    #[tokio::test]
    async fn session_ending_before_first_frame_still_sends_a_screen() {
        let (mut host, mut rx) = host();
        host.shell = "/usr/bin/true".into();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::new();
        let reply = c.send(
            &host,
            TermCmd::Attach {
                worktree_path: dir.path().to_path_buf(),
                slot: None,
                rows: 10,
                cols: 40,
            },
        );
        assert!(reply.is_none());
        let session = c.session();
        // Let the shell exit before any frame is taken.
        wait_until("session end", || ended(&session)).await;
        while next_sessions(&mut rx).await.0 != Vec::<u8>::new() {}
        assert!(matches!(
            c.frame(WAIT).await,
            Some(SupervisorMsg::TermScreen { slot: 1, .. })
        ));
        assert!(c.conn.view.is_none(), "the view ends after that screen");
    }

    #[test]
    fn tiny_size_is_clamped_to_a_safe_grid() {
        let (rows, cols) = clamp_size(0, 0);
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process("漢字 wraps\r\n漢字\r\n".as_bytes());
    }

    #[test]
    fn lowest_free_slot_reuses_gaps() {
        assert_eq!(lowest_free_slot(&[]), Some(1));
        assert_eq!(lowest_free_slot(&[1, 2, 3]), Some(4));
        assert_eq!(lowest_free_slot(&[1, 3]), Some(2));
        assert_eq!(lowest_free_slot(&[2, 3]), Some(1));
        assert_eq!(lowest_free_slot(&[1, 2, 3, 4, 5, 6, 7, 8, 9]), None);
    }

    #[tokio::test]
    async fn spawn_runs_in_worktree_cwd() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 200)).await;
        c.input(&host, "pwd\r");
        let name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        c.until(&format!("{name}\n")).await;
    }

    #[tokio::test]
    async fn resize_reaches_child() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        c.send(
            &host,
            TermCmd::Resize {
                rows: 37,
                cols: 101,
            },
        );
        c.input(&host, "stty size\r");
        c.until("37 101").await;
        assert_eq!(c.term.as_ref().unwrap().screen().size(), (37, 101));
    }

    #[tokio::test]
    async fn diff_stream_reproduces_daemon_screen() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        c.converge().await;

        // Needles use $((..)) so the echoed command line never matches them.
        // Scroll region: lines scroll inside rows 5..10 only.
        c.input(
            &host,
            "printf '\\033[5;10r\\033[10;1H'; for i in 1 2 3 4 5 6 7 8; do echo region-$i; done; printf '\\033[r\\033[20;1H'; echo step-$((0+1))\r",
        );
        c.until("step-1").await;
        c.converge().await;
        assert!(c.contents().contains("region-8") && !c.contents().contains("region-1\n"));

        // Alt-screen enter + drawing.
        c.input(
            &host,
            "printf '\\033[?1049h\\033[H\\033[2Jalt-draw\\033[3;7Hxy'; sleep 1; echo step-$((1+1))\r",
        );
        c.until("alt-draw").await;
        c.converge().await;
        assert!(
            !c.contents().contains("step-1"),
            "alt screen shows main content"
        );

        // Mid-stream resize (while on the alt screen).
        c.send(&host, TermCmd::Resize { rows: 30, cols: 90 });
        c.until("step-2").await;
        c.converge().await;
        assert_eq!(c.term.as_ref().unwrap().screen().size(), (30, 90));

        // Alt-screen exit restores the main screen.
        c.input(&host, "printf '\\033[?1049l'; echo step-$((1+2))\r");
        c.until("step-3").await;
        c.converge().await;
        assert!(c.contents().contains("step-1"), "main screen not restored");
        assert!(
            !c.contents().contains("step-2"),
            "alt screen content leaked"
        );
    }

    #[tokio::test]
    async fn detach_then_reattach_gets_full_screen() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        let slot = c.slot.unwrap();
        let session = c.session();
        c.input(&host, "sleep 1; echo away-$((40+2))\r");
        c.send(&host, TermCmd::Detach);
        assert!(c.conn.view.is_none());
        wait_until("output while detached", || {
            session
                .state
                .lock()
                .unwrap()
                .parser
                .screen()
                .contents()
                .contains("away-42")
        })
        .await;

        c.attach_again(&host, dir.path(), Some(slot), (24, 80))
            .await;
        assert!(c.contents().contains("away-42"));
        c.converge().await;
    }

    #[tokio::test]
    async fn kill_frees_slot_immediately() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut a = Client::attach(&host, dir.path(), None, (24, 80)).await;
        assert_eq!(a.slot, Some(1));
        let reply = a.send(
            &host,
            TermCmd::Kill {
                worktree_path: dir.path().to_path_buf(),
                slot: 1,
            },
        );
        assert!(reply.is_none());
        assert!(slots(&host, dir.path()).is_empty());
        let b = Client::attach(&host, dir.path(), None, (24, 80)).await;
        assert_eq!(b.slot, Some(1));
    }

    #[tokio::test]
    async fn hung_up_master_does_not_spin() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        // Natural exit.
        let mut a = Client::attach(&host, dir.path(), None, (24, 80)).await;
        let exited = a.session();
        a.input(&host, "exit\r");
        // Kill.
        let b = Client::attach(&host, dir.path(), None, (24, 80)).await;
        let killed = b.session();
        host.kill_worktree(dir.path());

        let start = std::time::Instant::now();
        wait_until("both tasks end", || ended(&exited) && ended(&killed)).await;
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "took {:?}",
            start.elapsed()
        );
        for s in [&exited, &killed] {
            let loops = s.loops.load(Ordering::Relaxed);
            assert!(loops < 1000, "session loop spun {loops} times");
        }
    }

    #[tokio::test]
    async fn flood_coalesces_and_converges() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        let start = Instant::now();
        let before = c.frames;
        c.input(&host, "yes | head -c 2000000; echo flood-$((40+2))\r");
        c.until("flood-42").await;
        c.converge().await;
        let frames = c.frames - before;
        let bound = (start.elapsed().as_millis() / FRAME.as_millis()) as usize + 2;
        assert!(frames <= bound, "{frames} frames > gate bound {bound}");
    }

    #[tokio::test]
    async fn two_viewers_both_receive_frames() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut a = Client::attach(&host, dir.path(), None, (24, 80)).await;
        let mut b = Client::attach(&host, dir.path(), a.slot, (24, 80)).await;
        a.input(&host, "echo both-$((40+2))\r");
        a.until("both-42").await;
        b.until("both-42").await;
        a.converge().await;
        b.converge().await;
    }

    #[tokio::test]
    async fn input_from_other_conn_takes_size_ownership() {
        let (host, _rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut a = Client::attach(&host, dir.path(), None, (24, 80)).await;
        let mut b = Client::attach(&host, dir.path(), a.slot, (30, 100)).await;
        // A owns the size and still views it, so B's attach does not resize.
        assert_eq!(b.daemon_screen().size(), (24, 80));

        b.input(&host, "stty size\r");
        b.until("30 100").await;
        assert_eq!(b.daemon_screen().size(), (30, 100));

        a.input(&host, "clear; stty size\r");
        a.until("24 80").await;
        assert_eq!(a.daemon_screen().size(), (24, 80));
    }

    #[tokio::test]
    async fn create_reply_carries_chosen_slot() {
        let (host, mut rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let a = Client::attach(&host, dir.path(), None, (24, 80)).await;
        assert_eq!(next_sessions(&mut rx).await.0, vec![1]);
        let b = Client::attach(&host, dir.path(), None, (24, 80)).await;
        assert_eq!(next_sessions(&mut rx).await.0, vec![1, 2]);
        assert_eq!((a.slot, b.slot), (Some(1), Some(2)));
    }

    #[tokio::test]
    async fn kill_reaps_and_frees_slot() {
        let (host, mut rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        next_sessions(&mut rx).await;
        let session = c.session();
        let shell = Pid::from_raw(session.pid as i32);
        c.input(&host, "sleep 100\r");
        let fd = session.state.lock().unwrap().master.unwrap();
        // SAFETY: the master stays open until the kill below.
        let fg_of = || tcgetpgrp(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }).unwrap();
        wait_until("sleep in the foreground", || fg_of() != shell).await;
        let fg = fg_of();

        let reply = c.send(
            &host,
            TermCmd::Kill {
                worktree_path: dir.path().to_path_buf(),
                slot: 1,
            },
        );
        assert!(reply.is_none());
        assert_eq!(next_sessions(&mut rx).await.0, Vec::<u8>::new());
        wait_until("shell reaped", || !is_alive(shell)).await;
        wait_until("foreground job gone", || killpg(fg, None).is_err()).await;
        assert!(ended(&session));
    }

    #[tokio::test]
    async fn exit_frees_slot() {
        let (host, mut rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        assert_eq!(next_sessions(&mut rx).await.0, vec![1]);
        c.input(&host, "exit\r");
        assert_eq!(next_sessions(&mut rx).await.0, Vec::<u8>::new());
        assert!(slots(&host, dir.path()).is_empty());
        // The viewer's frame stream ends and drops the view.
        let deadline = Instant::now() + WAIT;
        while c.conn.view.is_some() {
            assert!(Instant::now() < deadline, "view never ended");
            c.frame(Duration::from_millis(200)).await;
        }
    }

    #[tokio::test]
    async fn tenth_create_rejected_per_conn() {
        let (host, mut rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::new();
        for _ in 0..9 {
            c.attach_again(&host, dir.path(), None, (24, 80)).await;
        }
        assert_eq!(slots(&host, dir.path()), (1..=9).collect::<Vec<u8>>());
        let reply = c.send(
            &host,
            TermCmd::Attach {
                worktree_path: dir.path().to_path_buf(),
                slot: None,
                rows: 24,
                cols: 80,
            },
        );
        assert_eq!(
            reply,
            Some(SupervisorMsg::TermError {
                worktree_path: dir.path().to_path_buf(),
                message: "max 9 sessions".into()
            })
        );
        // The error is never broadcast.
        while let Ok(msg) = rx.try_recv() {
            assert!(!matches!(msg, SupervisorMsg::TermError { .. }));
        }
        let missing = c.send(
            &host,
            TermCmd::Kill {
                worktree_path: dir.path().join("nope"),
                slot: 4,
            },
        );
        assert!(
            matches!(missing, Some(SupervisorMsg::TermError { message, .. }) if message == "no session 4")
        );
        host.kill_worktree(dir.path());
    }

    #[tokio::test]
    async fn bell_sets_attention_only_when_unwatched() {
        let (host, mut rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        next_sessions(&mut rx).await;
        let session = c.session();

        // Watched: a bell does not mark the session.
        c.input(&host, "printf '\\a'; echo rang-$((40+2))\r");
        c.until("rang-42").await;
        assert!(!session.state.lock().unwrap().attention);

        // Unwatched: it does, and the edge is broadcast.
        c.input(&host, "sleep 1; printf '\\a'\r");
        c.send(&host, TermCmd::Detach);
        assert_eq!(next_sessions(&mut rx).await, (vec![1], true));
        assert!(session.state.lock().unwrap().attention);
    }

    #[tokio::test]
    async fn attach_clears_attention() {
        let (host, mut rx) = host();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::attach(&host, dir.path(), None, (24, 80)).await;
        next_sessions(&mut rx).await;
        c.input(&host, "sleep 1; printf '\\a'\r");
        c.send(&host, TermCmd::Detach);
        assert_eq!(next_sessions(&mut rx).await, (vec![1], true));

        c.attach_again(&host, dir.path(), Some(1), (24, 80)).await;
        assert_eq!(next_sessions(&mut rx).await, (vec![1], false));
        assert!(!c.session().state.lock().unwrap().attention);
    }
}

/// The OS behaviour `PtyHost` relies on, checked on every platform CI runs:
/// AsyncFd on a PTY master, `tcgetpgrp(master)` naming the foreground job,
/// EOF/EIO after hangup, and vt100 never emitting `?1049`.
#[cfg(test)]
mod spike {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::{tcgetpgrp, Pid};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt as _;
    use std::time::{Duration, Instant};
    use tokio::io::unix::AsyncFd;

    async fn read_until(afd: &AsyncFd<std::fs::File>, out: &mut Vec<u8>, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(out).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timeout waiting for {needle:?}: {:?}",
                String::from_utf8_lossy(out)
            );
            let mut g = tokio::time::timeout(Duration::from_secs(10), afd.readable())
                .await
                .unwrap()
                .unwrap();
            let mut buf = [0u8; 4096];
            if let Ok(Ok(n)) = g.try_io(|f| f.get_ref().read(&mut buf)) {
                out.extend_from_slice(&buf[..n]);
            }
        }
    }

    #[tokio::test]
    async fn spike_pty_on_this_os() {
        let pty = nix::pty::openpty(None, None).unwrap();
        fcntl(pty.master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.stdin(pty.slave.try_clone().unwrap())
            .stdout(pty.slave.try_clone().unwrap())
            .stderr(pty.slave);
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = tokio::process::Command::from(cmd).spawn().unwrap();
        let shell = Pid::from_raw(child.id().unwrap() as i32);
        let afd = AsyncFd::new(std::fs::File::from(pty.master)).unwrap();
        let mut out = Vec::new();

        // (i) echo round-trip.
        afd.get_ref().write_all(b"echo hi-$((1+1))\r").unwrap();
        read_until(&afd, &mut out, "hi-2").await;

        // (ii) tcgetpgrp(master) reports the foreground job, not the shell.
        afd.get_ref().write_all(b"sleep 100\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let fg = loop {
            let fg = tcgetpgrp(afd.get_ref()).unwrap();
            if fg != shell {
                break fg;
            }
            assert!(Instant::now() < deadline, "fg pgrp never left the shell");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let ps = std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &fg.to_string()])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&ps.stdout).contains("sleep"),
            "fg pgrp leader is not sleep"
        );

        // (iii) hangup: after the shell dies the master returns EIO/0 and stays readable.
        let _ = killpg(fg, Signal::SIGHUP);
        let _ = killpg(shell, Signal::SIGHUP);
        // Keep draining: macOS blocks the exiting shell's tty close until output drains.
        let mut eof_hits = 0;
        let mut exited = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(exited && eof_hits >= 3) {
            assert!(
                Instant::now() < deadline,
                "exited={exited} eof_hits={eof_hits}"
            );
            exited = exited || child.try_wait().unwrap().is_some();
            let mut g = tokio::time::timeout(Duration::from_secs(5), afd.readable())
                .await
                .unwrap()
                .unwrap();
            let mut buf = [0u8; 4096];
            match g.try_io(|f| f.get_ref().read(&mut buf)) {
                Ok(Ok(0)) => eof_hits += 1,
                Ok(Err(e)) if e.raw_os_error() == Some(nix::libc::EIO) => eof_hits += 1,
                Ok(Ok(_)) | Err(_) => {}
                Ok(Err(e)) => panic!("unexpected read error after hangup: {e}"),
            }
        }

        // (iv) vt100 never emits ?1049 from state_formatted/state_diff.
        let mut p = vt100::Parser::new(24, 80, 0);
        let before = p.screen().clone();
        p.process(b"main\x1b[?1049halt-screen\x1b[5;10r");
        assert!(p.screen().alternate_screen());
        let f = p.screen().state_formatted();
        let d = p.screen().state_diff(&before);
        let has = |b: &[u8]| b.windows(5).any(|w| w == b"?1049");
        assert!(!has(&f) && !has(&d), "vt100 emitted ?1049");

        // (v) widget signature compiles.
        let _w = tui_term::widget::PseudoTerminal::new(p.screen());
    }
}
