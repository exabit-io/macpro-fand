//! The small Linux ABI surface needed for ownership and service integration.
use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        linux::net::SocketAddrExt,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{SocketAddr, UnixDatagram},
        },
    },
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("macpro-fand targets the x86-64 Mac Pro 7,1 on Linux; the Linux ABI constants are verified for this target");
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn geteuid() -> u32;
    fn getppid() -> i32;
    fn signal(sig: i32, handler: usize) -> usize;
}
const O_NOFOLLOW: i32 = 0o400000;
const O_CLOEXEC: i32 = 0o2000000;
const O_NONBLOCK: i32 = 0o4000;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
pub static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn request_stop(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}
pub fn install_signals() -> io::Result<()> {
    // SAFETY: Linux signal ABI; the handler only stores a lock-free atomic flag.
    unsafe {
        for sig in [2, 15] {
            if signal(sig, request_stop as usize) == usize::MAX {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
pub fn stopped() -> bool {
    STOP.load(Ordering::SeqCst)
}
pub fn uid() -> u32 {
    unsafe { geteuid() }
}
pub fn under_systemd() -> bool {
    std::env::var_os("INVOCATION_ID").is_some() && unsafe { getppid() } == 1
}
/// How often a sleeping control loop looks at the stop flag. The flag is set by the signal handler, which cannot
/// wake a condition variable, so the loop polls it -- at a rate that costs two wakeups a second, with a stop
/// latency of at most half a second (1.0.6 noticed a stop only at the end of its 2 s period).
pub const STOP_POLL: Duration = Duration::from_millis(500);

pub fn sleep_interruptible(duration: Duration) {
    let end = Instant::now() + duration;
    while !stopped() {
        let Some(left) = end.checked_duration_since(Instant::now()) else {
            break;
        };
        thread::sleep(left.min(STOP_POLL));
    }
}

/// Never truncate, follow a symlink, wait opening a FIFO, or use another user's lock.
/// The directory and file must both belong to this uid, with no writable directory
/// permissions for other users. The fd stays open for the ownership lifetime.
pub struct Lock {
    file: File,
}
impl Lock {
    pub fn open(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lock has no directory"))?;
        match fs::create_dir(parent) {
            Ok(()) => {
                fs::set_permissions(parent, Permissions::from_mode(0o755))?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let directory = fs::symlink_metadata(parent)?;
        if !directory.is_dir() || directory.uid() != uid() || directory.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "lock directory must be owned by this uid and not writable by other users",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
            .open(path)?;
        let m = file.metadata()?;
        if !m.is_file() || m.uid() != uid() || m.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "lock must be a regular, singly linked file owned by this uid",
            ));
        }
        file.set_permissions(Permissions::from_mode(0o600))?;
        Ok(Self { file })
    }
    pub fn acquire(&self, timeout: Duration) -> io::Result<bool> {
        let end = Instant::now() + timeout;
        loop {
            // SAFETY: file owns a valid descriptor for this entire call.
            if unsafe { flock(self.file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
                return Ok(true);
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(11) | Some(4) => {}
                _ => return Err(e),
            }
            let Some(left) = end.checked_duration_since(Instant::now()) else {
                return Ok(false);
            };
            thread::sleep(left.min(Duration::from_millis(10)));
        }
    }
}
pub fn notify(message: &str) -> io::Result<()> {
    let Some(address) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    use std::os::unix::ffi::OsStrExt;
    let bytes = address.as_bytes();
    let socket = UnixDatagram::unbound()?;
    socket.set_write_timeout(Some(Duration::from_millis(100)))?;
    if let Some(name) = bytes.strip_prefix(b"@") {
        let addr = SocketAddr::from_abstract_name(name)?;
        socket.send_to_addr(message.as_bytes(), &addr)?;
    } else {
        socket.send_to(message.as_bytes(), Path::new(&address))?;
    }
    Ok(())
}
#[derive(Debug)]
pub struct CommandResult {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
}
pub fn command(program: &str, args: &[&str], timeout: Duration) -> io::Result<CommandResult> {
    let end = Instant::now() + timeout;
    let mut child = Command::new(program)
        .args(args)
        .env_remove("NOTIFY_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    // Drain output concurrently so a full pipe cannot stall the child. Output is bounded.
    if let Err(e) = thread::Builder::new()
        .name("service-output".into())
        .spawn(move || {
            let mut b = Vec::new();
            let _ = stdout.take(65537).read_to_end(&mut b);
            let _ = tx.send(b);
        })
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    loop {
        if let Some(status) = child.try_wait()? {
            let remaining = end.saturating_duration_since(Instant::now());
            let output = rx.recv_timeout(remaining).unwrap_or_default();
            return Ok(CommandResult {
                success: status.success(),
                code: status.code(),
                stdout: String::from_utf8_lossy(&output).into_owned(),
            });
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}
pub trait Services {
    fn active(&mut self, unit: &str) -> io::Result<bool>;
    fn exists(&mut self, unit: &str) -> io::Result<bool>;
    fn restarting(&mut self) -> io::Result<bool>;
    fn start(&mut self, unit: &str) -> io::Result<bool>;
}
pub struct Systemd;
impl Systemd {
    fn ctl(&self, args: &[&str]) -> io::Result<CommandResult> {
        command("/usr/bin/systemctl", args, Duration::from_secs(10))
    }
}
impl Services for Systemd {
    fn active(&mut self, unit: &str) -> io::Result<bool> {
        let r = self.ctl(&["is-active", "--quiet", unit])?;
        match r.code {
            Some(0) => Ok(true),
            Some(3) | Some(4) => Ok(false), // inactive/failed, or unknown unit
            _ => Err(io::Error::other(
                "cannot determine whether the fan service is active",
            )),
        }
    }
    fn exists(&mut self, unit: &str) -> io::Result<bool> {
        let r = self.ctl(&["show", "--property=LoadState", "--value", unit])?;
        if !r.success {
            return Err(io::Error::other(
                "cannot determine whether the fan service is installed",
            ));
        }
        match r.stdout.trim() {
            "loaded" => Ok(true),
            "not-found" => Ok(false),
            _ => Err(io::Error::other(
                "fan service is masked, invalid, or has unknown load state",
            )),
        }
    }
    fn start(&mut self, unit: &str) -> io::Result<bool> {
        Ok(self.ctl(&["start", "--no-block", unit])?.success)
    }
    fn restarting(&mut self) -> io::Result<bool> {
        let text = fs::read_to_string("/proc/self/cgroup")?;
        let Some(unit) = text
            .trim()
            .split('/')
            .rev()
            .find(|c| c.ends_with(".service"))
        else {
            return Ok(false);
        };
        let jobs = self.ctl(&["list-jobs", "--no-legend", "--no-pager", unit])?;
        if !jobs.success {
            return Err(io::Error::other("cannot determine pending service jobs"));
        }
        Ok(jobs
            .stdout
            .split_whitespace()
            .any(|s| ["start", "restart", "try-restart", "reload-or-start"].contains(&s)))
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    static TEMP_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    pub(crate) fn temp() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "macpro-fand-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn ownership_conflict_symlink_and_permissions() {
        let d = temp();
        let p = d.join("owner.lock");
        let a = Lock::open(&p).unwrap();
        assert!(a.acquire(Duration::ZERO).unwrap());
        let b = Lock::open(&p).unwrap();
        assert!(!b.acquire(Duration::from_millis(10)).unwrap());
        drop(a);
        assert!(b.acquire(Duration::ZERO).unwrap());
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        drop(b);
        fs::remove_file(&p).unwrap();
        fs::write(d.join("target"), "keep").unwrap();
        std::os::unix::fs::symlink(d.join("target"), &p).unwrap();
        assert!(Lock::open(&p).is_err());
        assert_eq!(fs::read_to_string(d.join("target")).unwrap(), "keep");
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn refuses_shared_writable_directory_and_hardlink() {
        let d = temp();
        let p = d.join("owner.lock");
        fs::write(d.join("target"), "keep").unwrap();
        fs::hard_link(d.join("target"), &p).unwrap();
        assert!(Lock::open(&p).is_err());
        fs::remove_file(&p).unwrap();
        fs::set_permissions(&d, Permissions::from_mode(0o777)).unwrap();
        assert!(Lock::open(&p).is_err());
        fs::remove_dir_all(d).unwrap();
    }
}
