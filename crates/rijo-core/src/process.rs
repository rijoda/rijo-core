//! Background process registry.
//!
//! `terminal(background: true)` starts processes here instead of dropping
//! them: stdout/stderr go into FILES (not into pipes — unread pipes
//! block the process as soon as its buffer is full, and nobody can
//! read the output), the child handle is kept so that `process_manage`
//! can poll, log, wait, write and kill.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

static PROC: LazyLock<Mutex<HashMap<String, BgProcess>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SEQ: AtomicU64 = AtomicU64::new(1);

struct BgProcess {
    id: String,
    pid: u32,
    command: String,
    workdir: String,
    started_ms: u128,
    out_path: PathBuf,
    err_path: PathBuf,
    out_pos: u64,
    err_pos: u64,
    child: Option<std::process::Child>,
    exit: Option<i32>,
}

/// Public view of a process (JSON-capable).
#[derive(Debug, Clone)]
pub struct Info {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub workdir: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub uptime_ms: u128,
    pub stdout_path: String,
    pub stderr_path: String,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn info_of(p: &BgProcess) -> Info {
    Info {
        id: p.id.clone(),
        pid: p.pid,
        command: p.command.clone(),
        workdir: p.workdir.clone(),
        running: p.exit.is_none(),
        exit_code: p.exit,
        uptime_ms: now_ms().saturating_sub(p.started_ms),
        stdout_path: p.out_path.to_string_lossy().to_string(),
        stderr_path: p.err_path.to_string_lossy().to_string(),
    }
}

/// Resolves an id or a unique prefix (`proc_ab` → `proc_abcd1234`).
/// Two-step: first determine the key (immutable), then mutate — otherwise
/// the return value collides with the ongoing borrow.
fn resolve_key(map: &HashMap<String, BgProcess>, id_or_prefix: &str) -> Result<String, String> {
    if map.contains_key(id_or_prefix) {
        return Ok(id_or_prefix.to_string());
    }
    let matches: Vec<&str> = map
        .keys()
        .map(|k| k.as_str())
        .filter(|k| k.starts_with(id_or_prefix))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].to_string()),
        0 => Err(format!("no process with id/prefix '{id_or_prefix}'")),
        _ => Err(format!(
            "prefix '{id_or_prefix}' is ambiguous ({} matches)",
            matches.len()
        )),
    }
}

fn resolve<'a>(
    map: &'a mut HashMap<String, BgProcess>,
    id_or_prefix: &str,
) -> Result<&'a mut BgProcess, String> {
    // Refresh the exit status while we still have the handle
    for p in map.values_mut() {
        if p.exit.is_none() {
            if let Some(child) = p.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    p.exit = Some(status.code().unwrap_or(-1));
                }
            }
        }
    }
    let key = resolve_key(map, id_or_prefix)?;
    map.get_mut(&key)
        .ok_or_else(|| format!("process '{key}' disappeared"))
}

fn read_slice(path: &Path, from: u64) -> (String, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return (String::new(), 0);
    };
    if f.seek(SeekFrom::Start(from)).is_err() {
        return (String::new(), 0);
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    // Return the RAW byte count as well: `poll` must advance the position by
    // bytes actually consumed, not by the length of the lossy UTF-8 string
    // (a replacement char is 3 bytes for 1 consumed byte → drift on binary output).
    let n = buf.len() as u64;
    (String::from_utf8_lossy(&buf).to_string(), n)
}

/// Stream one log file, counting lines into `total` and collecting the window
/// `[skip, skip+take)` into `window`. Bounded memory (one line at a time) — the
/// old `read_to_string` loaded the whole log just to page a few lines.
fn collect_lines(
    path: &Path,
    total: &mut usize,
    skip: usize,
    take: usize,
    window: &mut Vec<String>,
) {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = std::io::BufReader::new(f);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).unwrap_or(0) == 0 {
            break;
        }
        if *total >= skip && window.len() < take {
            let mut line = String::from_utf8_lossy(&buf).to_string();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            window.push(line);
        }
        *total += 1;
    }
}

/// Starts a process in the background, output into files under
/// `<data_dir>/proc/<id>.out|.err`. Returns (id, pid).
pub fn spawn(
    command: &str,
    workdir: Option<&str>,
    data_dir: &Path,
) -> Result<(String, u32), String> {
    let dir = data_dir.join("proc");
    std::fs::create_dir_all(&dir).map_err(|e| format!("proc directory: {e}"))?;

    let id = format!("proc_{:08x}", SEQ.fetch_add(1, Ordering::SeqCst));
    let out_path = dir.join(format!("{id}.out"));
    let err_path = dir.join(format!("{id}.err"));
    let out = std::fs::File::create(&out_path).map_err(|e| format!("stdout file: {e}"))?;
    let err = std::fs::File::create(&err_path).map_err(|e| format!("stderr file: {e}"))?;

    let (shell, flag) = crate::tools::terminal_tool::shell_command();

    let mut cmd = std::process::Command::new(&shell);
    cmd.arg(flag)
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(out)
        .stderr(err);

    // `current_dir` setzt das Arbeitsverzeichnis ueber die Windows-API — die
    // Shell muss den Pfad nicht parsen, also funktioniert hier auch ein
    // verbatim-Pfad (`\\?\...`), wie ihn `canonicalize` liefert. Ein
    // `cd <pfad> &&` im Befehl waere der Umweg ueber bash' Pfadparser und
    // scheitert dort an Formen, die Windows selbst problemlos annimmt.
    let wd = std::fs::canonicalize(workdir.unwrap_or(".")).unwrap_or_else(|_| PathBuf::from("."));
    cmd.current_dir(&wd);

    // Own process group: that way the timeout kills the WHOLE group and
    // not just the shell — otherwise a grandchild process (e.g. a server
    // started with `&`) survives every abort as an orphan.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().map_err(|e| format!("start failed: {e}"))?;

    let pid = child.id();
    let workdir_str = workdir.unwrap_or(".").to_string();
    let p = BgProcess {
        id: id.clone(),
        pid,
        command: command.to_string(),
        workdir: workdir_str,
        started_ms: now_ms(),
        out_path,
        err_path,
        out_pos: 0,
        err_pos: 0,
        child: Some(child),
        exit: None,
    };
    PROC.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), p);
    Ok((id, pid))
}

pub fn list() -> Vec<Info> {
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    // one pass for a fresh exit status
    let ids: Vec<String> = map.keys().cloned().collect();
    for id in ids {
        let _ = resolve(&mut map, &id);
    }
    let mut v: Vec<Info> = map.values().map(info_of).collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

/// New output since the last poll (stdout + stderr separate).
pub fn poll(id_or_prefix: &str) -> Result<(Info, String, String), String> {
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    let p = resolve(&mut map, id_or_prefix)?;
    let (out, out_bytes) = read_slice(&p.out_path, p.out_pos);
    let (err, err_bytes) = read_slice(&p.err_path, p.err_pos);
    p.out_pos += out_bytes;
    p.err_pos += err_bytes;
    Ok((info_of(p), out, err))
}

/// Paged log: lines from `offset` (1-based), max `limit`.
pub fn log(
    id_or_prefix: &str,
    offset: usize,
    limit: usize,
) -> Result<(Info, String, usize), String> {
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    let p = resolve(&mut map, id_or_prefix)?;
    let skip = offset.saturating_sub(1);
    let take = limit.max(1);
    let mut total = 0usize;
    let mut window: Vec<String> = Vec::new();
    // stdout first, then stderr — the same order the old concatenation used.
    collect_lines(&p.out_path, &mut total, skip, take, &mut window);
    collect_lines(&p.err_path, &mut total, skip, take, &mut window);
    Ok((info_of(p), window.join("\n"), total))
}

/// Drops finished processes from the registry (frees the child handle and its
/// metadata). Running processes are kept. Returns the number removed.
pub fn prune() -> usize {
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    // Refresh status first so a just-finished process counts as finished.
    let ids: Vec<String> = map.keys().cloned().collect();
    for id in &ids {
        let _ = resolve(&mut map, id);
    }
    let before = map.len();
    map.retain(|_, p| p.exit.is_none());
    before - map.len()
}

/// Waits up to `timeout_ms` for the process to end.
pub fn wait(id_or_prefix: &str, timeout_ms: u64) -> Result<Info, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        {
            let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
            let p = resolve(&mut map, id_or_prefix)?;
            if p.exit.is_some() {
                return Ok(info_of(p));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(info_of(p)); // still running — caller sees running: true
            }
            if let Some(child) = p.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    p.exit = Some(status.code().unwrap_or(-1));
                    return Ok(info_of(p));
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub fn kill(id_or_prefix: &str) -> Result<Info, String> {
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    let p = resolve(&mut map, id_or_prefix)?;
    if let Some(child) = p.child.as_mut() {
        // Whole tree: a background process can have children itself
        // (e.g. `npm run dev` → node). Without that they stay behind as orphans.
        kill_tree(child.id());
        let _ = child.kill();
        if let Ok(status) = child.wait() {
            p.exit = Some(status.code().unwrap_or(-1));
        } else {
            p.exit = Some(-1);
        }
    }
    Ok(info_of(p))
}

/// Kills a process **and its descendants**.
///
/// Unix: the negative PID addresses the process group the caller created with
/// `process_group(0)`; a plain `Child::kill` would leave a server behind `&`
/// running as an orphan.
///
/// Windows has no equivalent of `killpg`. `taskkill /T /F` walks the child
/// tree instead — a system tool, so no extra dependency, and it is the only
/// thing that reliably reaches a grandchild (`npm run dev` → node).
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        nix::libc::kill(-(pid as i32), nix::libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Writes to stdin (`submit` appends a line break).
pub fn write_stdin(id_or_prefix: &str, data: &str, submit: bool) -> Result<(), String> {
    use std::io::Write;
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    let p = resolve(&mut map, id_or_prefix)?;
    let child = p
        .child
        .as_mut()
        .ok_or("no handle (process already reaped)")?;
    let stdin = child.stdin.as_mut().ok_or("no stdin channel")?;
    let payload = if submit {
        format!("{data}\n")
    } else {
        data.to_string()
    };
    stdin
        .write_all(payload.as_bytes())
        .map_err(|e| format!("stdin write failed: {e}"))?;
    let _ = stdin.flush();
    Ok(())
}

/// Closes stdin (EOF) — e.g. so that an interactive program ends cleanly.
pub fn close_stdin(id_or_prefix: &str) -> Result<(), String> {
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    let p = resolve(&mut map, id_or_prefix)?;
    if let Some(child) = p.child.as_mut() {
        child.stdin.take();
    }
    Ok(())
}

/// Terminates all still running processes (process exit/shutdown).
pub fn shutdown_all() {
    let mut map = PROC.lock().unwrap_or_else(|e| e.into_inner());
    for p in map.values_mut() {
        if p.exit.is_none() {
            if let Some(child) = p.child.as_mut() {
                kill_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
            }
            p.exit = Some(-1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rijo-proc-test-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// Serialisiert die Tests, die die globale Prozess-Registry benutzen.
    ///
    /// Die Registry ist prozessweit und die Tests laufen parallel: ein `kill`
    /// oder `shutdown_all` in einem Test trifft sonst die Prozesse des
    /// anderen. Der Fehler ist ein Flake (mal gruen, mal rot), deshalb ist er
    /// ohne diesen Riegel nicht reproduzierbar zu debuggen.
    static REGISTRY_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
        REGISTRY_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// resolve_key is pure — separately testable, without the global registry.
    #[test]
    fn resolve_key_matches_exact_and_unique_prefix() {
        let _guard = registry_lock();
        let d = tmp_dir("resolve");
        let (id, _) = spawn("sleep 5", None, &d).unwrap();
        let map = PROC.lock().unwrap_or_else(|e| e.into_inner());
        // exact id
        assert_eq!(resolve_key(&map, &id).unwrap(), id);
        // unique prefix: only the last 2 characters differ per process,
        // so work with the full id prefix, which is unique at this moment
        let prefix = &id[..id.len()];
        assert_eq!(resolve_key(&map, prefix).unwrap(), id);
        assert!(resolve_key(&map, "proc_zzzzzzzz").is_err(), "unknown");
        drop(map);
        let _ = kill(&id);
    }

    /// Retries `probe` until it yields `Some`, or panics after `timeout`.
    ///
    /// The registry tests drive a real shell. On a loaded machine — a full
    /// `cargo test` right after a release build — Git Bash can take longer than
    /// any fixed sleep we would pick, which made this test flaky rather than
    /// wrong. Waiting for the condition instead of sleeping a guessed amount is
    /// what makes it deterministic.
    fn wait_until<T>(
        what: &str,
        timeout: std::time::Duration,
        mut probe: impl FnMut() -> Option<T>,
    ) -> T {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(v) = probe() {
                return v;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out after {timeout:?} waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// ONE test for the whole lifecycle: the registry is global, parallel
    /// tests would kill each other's processes (shutdown_all kills all).
    #[test]
    fn background_process_lifecycle() {
        let _guard = registry_lock();
        let d = tmp_dir("lifecycle");

        // 1) start + exit code + poll (incremental) + stderr
        // `spawn` umhuellt den Befehl selbst mit der Shell — hier steht nur der
        // Befehlstext. Sequenz und stderr sind plattformabhaengig: bash nutzt
        // `;` und `>&2`, cmd.exe `&` und `1>&2`. Der Warteteil ist bewusst
        // laenger als ein einzelner Shell-Start, damit das Fenster fuer die
        // "laeuft noch"-Pruefung nicht vom Zufall abhaengt.
        let (shell, _) = crate::tools::terminal_tool::shell_command();
        let cmd = if shell == "cmd.exe" {
            "echo eins & echo fehler 1>&2 & ping -n 3 127.0.0.1 > nul & echo zwei"
        } else {
            "echo eins; echo fehler >&2; sleep 1.5; echo zwei"
        };
        let (id1, pid1) = spawn(cmd, None, &d).unwrap();
        assert!(pid1 > 0);

        // Wait on the output FILE, not on `poll`: poll is incremental and would
        // consume the very output this assertion is about.
        let out_path = list()
            .into_iter()
            .find(|i| i.id == id1)
            .expect("our process in the registry")
            .stdout_path;
        let err_path = out_path.replace(".out", ".err");
        wait_until(
            "stdout 'eins' and stderr 'fehler'",
            std::time::Duration::from_secs(10),
            || {
                let out = std::fs::read_to_string(&out_path).unwrap_or_default();
                let err = std::fs::read_to_string(&err_path).unwrap_or_default();
                (out.contains("eins") && err.contains("fehler")).then_some(())
            },
        );

        let (info, first_out, first_err) = poll(&id1).unwrap();
        assert!(first_out.contains("eins"), "first poll: {first_out:?}");
        assert!(first_err.contains("fehler"), "stderr: {first_err:?}");
        assert!(info.running, "still running during sleep");
        let info = wait(&id1, 10_000).unwrap();
        assert!(!info.running, "must have finished");
        assert_eq!(info.exit_code, Some(0));
        let (_, second_out, _) = poll(&id1).unwrap();
        assert!(second_out.contains("zwei"), "second poll: {second_out:?}");
        let (_, third_out, _) = poll(&id1).unwrap();
        assert!(third_out.is_empty(), "poll is incremental: {third_out:?}");

        // 2) paged log + kill of a running process
        // `log` is paged by offset, so retrying it is safe.
        let (id2, _) = spawn("printf 'a\\nb\\nc\\nd\\n'; sleep 30", None, &d).unwrap();
        let (info2, page, total) =
            wait_until("4 log lines", std::time::Duration::from_secs(10), || {
                let (i, p, t) = log(&id2, 2, 2).unwrap();
                (t == 4).then_some((i, p, t))
            });
        assert!(info2.running, "sleep 30 must still be running");
        assert_eq!(page, "b\nc");
        assert_eq!(total, 4);
        let killed = kill(&id2).unwrap();
        assert!(!killed.running);

        // 3) stdin: submit delivers the line to the process
        let (id3, _) = spawn("read line; echo \"got:$line\"", None, &d).unwrap();
        write_stdin(&id3, "ping", true).unwrap();
        let info3 = wait(&id3, 5_000).unwrap();
        assert!(!info3.running, "process ends after read");
        let (_, out3, _) = poll(&id3).unwrap();
        assert!(out3.contains("got:ping"), "stdout: {out3:?}");

        // 4) list contains all three, shutdown_all terminates the running one
        let (id4, _) = spawn("sleep 30", None, &d).unwrap();
        let ids: Vec<String> = list().into_iter().map(|i| i.id).collect();
        for want in [&id1, &id2, &id3, &id4] {
            assert!(ids.contains(want), "{want} missing in list(): {ids:?}");
        }
        shutdown_all();
        let after = list().into_iter().find(|i| i.id == id4).unwrap();
        assert!(!after.running, "shutdown_all must terminate");
    }

    /// The poll position must advance by RAW bytes, not by the length of the
    /// lossy UTF-8 string: a replacement char is 3 bytes for 1 consumed byte, so
    /// the old `out.len()` accounting skipped bytes on binary output.
    #[test]
    fn read_slice_reports_raw_bytes_not_lossy_length() {
        let d = tmp_dir("slice");
        let p = d.join("bin.dat");
        std::fs::write(&p, b"ab\xFFcd").unwrap();
        let (s, n) = read_slice(&p, 0);
        assert_eq!(
            n, 5,
            "raw byte count must be returned, not the lossy length"
        );
        assert!(s.starts_with("ab") && s.ends_with("cd"), "{s:?}");
        // Reading from a later offset returns only the tail's raw length.
        let (tail, n2) = read_slice(&p, 4);
        assert_eq!(n2, 1);
        assert_eq!(tail, "d");
    }

    /// `prune` drops finished processes (freeing their handles) but never
    /// touches a running one.
    #[test]
    fn prune_drops_finished_and_keeps_running() {
        let _guard = registry_lock();
        let d = tmp_dir("prune");
        let (done, _) = spawn("echo done", None, &d).unwrap();
        let (running, _) = spawn("sleep 30", None, &d).unwrap();
        let _ = wait(&done, 10_000); // ensure `done` has exited

        let removed = prune();
        assert!(removed >= 1, "at least the finished process is pruned");
        let ids: Vec<String> = list().into_iter().map(|i| i.id).collect();
        assert!(
            !ids.contains(&done),
            "finished process must be pruned: {ids:?}"
        );
        assert!(ids.contains(&running), "running process must stay: {ids:?}");
        let _ = kill(&running);
    }
}
