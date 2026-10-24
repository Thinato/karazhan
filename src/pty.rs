//! Daemon-owned PTY sessions (spike).

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
