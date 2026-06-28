use std::{
    convert::Infallible,
    ffi::CString,
    fs::{self},
    path::PathBuf,
    str::FromStr,
};

use anyhow::{Context, Ok, Result};
use nix::{
    mount::MsFlags,
    sched::CloneFlags,
    sys::wait::WaitStatus,
    unistd::{ForkResult, Gid, Pid, Uid},
};

fn main() -> Result<()> {
    println!("Starting tinox...");
    let tinox_info = get_process()?;
    print_process(&tinox_info);
    let location = setup_run_dir(&tinox_info.u_id)?;
    setup_parent_namespaces()?;
    map_uid_gid(&tinox_info.u_id, &tinox_info.g_id)?;
    match fork_and_wait_for_exit()? {
        ForkRes::Child => {
            isolate_child()?;
            change_hostname()?;
            change_filesystem(&location)?;
            let child_proc = get_process()?;
            print_process(&child_proc);
            run_command()?;
        }
        ForkRes::Parent(wait_status) => {
            match wait_status {
                WaitStatus::Exited(pid, code) => println!("Child({pid}) exited with code {code}"),
                _ => println!("Child not exited, something else..."),
            }
            println!("Tinox done!");
        }
    };
    Ok(())
}

fn setup_run_dir(uid: &Uid) -> Result<ContainerLocation> {
    let run_dir = PathBuf::from(format!("/run/user/{uid}/tinox/container"));
    let upper = run_dir.join("upper");
    let work = run_dir.join("work");
    let merged = run_dir.join("merged");

    if run_dir.exists() {
        // Overlayfs leaves work/work as an empty, unreadable directory after unmount.
        // remove_dir_all can't read into it, so it fails. Simply removing the dir still works
        // so we try to do that first before deleting al other folders
        let inner_work = work.join("work");
        if inner_work.exists() {
            fs::remove_dir(&inner_work).context("Failed to remove the work dir")?;
        }
        fs::remove_dir_all(&run_dir).context("Failed to cleanup previous run dir")?;
    }
    fs::create_dir_all(&run_dir).context("Failed to create run dir")?;

    for dir in [&upper, &work, &merged] {
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    }

    Ok(ContainerLocation {
        upper,
        work,
        merged,
    })
}

fn change_filesystem(location: &ContainerLocation) -> Result<()> {
    let options = format!(
        "lowerdir=fs/fs,upperdir={},workdir={}",
        &location.upper.to_string_lossy(),
        &location.work.to_string_lossy()
    );
    dbg!(&options);
    nix::mount::mount(
        Some("overlay"),
        &location.merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(options.as_str()),
    )
    .context("Failed to mount filesystem")?;
    nix::unistd::chdir(&location.merged).context("Failed to change to merged directory")?;
    nix::unistd::pivot_root(".", ".").context("Failed to pivot root")?;
    nix::unistd::chdir("/").context("Failed to change to root directory")?;
    nix::mount::mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .context("Failed to mount proc")
}

fn change_hostname() -> Result<()> {
    nix::unistd::sethostname("HELLO").context("Failed to set hostname")
}

fn setup_parent_namespaces() -> Result<()> {
    nix::sched::unshare(CloneFlags::CLONE_NEWUSER).context("Failed to isolate USER namespace")?;
    nix::sched::unshare(CloneFlags::CLONE_NEWPID).context("Failed to isolate PID namespace")
}

fn isolate_child() -> Result<()> {
    nix::sched::unshare(CloneFlags::CLONE_NEWUTS).context("Failed to isolate UTS namespace")?;
    nix::sched::unshare(CloneFlags::CLONE_NEWNS).context("Failed to isolate mount namespace")?;
    //By default '/' is mounted shared. So, altough we have a seperate mount table trough the unshare above,
    // any mount we add/remove under '/' will still be active for other namespaces.
    //We make it private (recursively) in our new namespace, so mounts we create actually stay local
    nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .context("Failed mounting root a private")
}

/// Replace the current process with the actual command we want to run
fn run_command() -> Result<Infallible> {
    let command = CString::from_str("/bin/busybox")?;
    let arg = CString::from_str("sh")?;
    nix::unistd::execv::<CString>(&command, &[arg]).context("Could not execv")
}

fn get_process() -> Result<ProcInfo> {
    let proc_id = nix::unistd::getpid();
    let parent_id = nix::unistd::getppid();
    let u_id = nix::unistd::getuid();
    let g_id = nix::unistd::getgid();
    let host_name = nix::unistd::gethostname()?
        .to_str()
        .unwrap_or("Empty")
        .to_string();
    let cwd = nix::unistd::getcwd()?
        .to_str()
        .unwrap_or("Empty")
        .to_string();
    Ok(ProcInfo {
        proc_id,
        parent_id,
        u_id,
        g_id,
        host_name,
        cwd,
    })
}
fn print_process(proc: &ProcInfo) {
    println!(
        "---ProcInfo---\nPID: {}\nPPID: {}\nUID: {}\nGID: {}\nHostname: {}\nCWD: {}",
        proc.proc_id, proc.parent_id, proc.u_id, proc.g_id, proc.host_name, proc.cwd
    );
}

/// Maps the current UID/GID to root
///
/// This allows us to further to run as root in our seperate namespace without having to run our container runner as root
/// We do this for both UID and GID. The setgroups thing is needed by the kernel to avoid some security issue.
fn map_uid_gid(uid: &Uid, gid: &Gid) -> Result<()> {
    let uid_map = format!("0 {} 1", uid);
    let gid_map = format!("0 {} 1", gid);
    fs::write("/proc/self/uid_map", uid_map).context("Failed to write UID map")?;
    fs::write("/proc/self/setgroups", "deny").context("Failed to set_groups")?;
    fs::write("/proc/self/gid_map", gid_map).context("Failed to write GID map")?;
    Ok(())
}

/// Fork the process and wait
///
/// Either this returns as the child process, or it blocks until the child has exited
fn fork_and_wait_for_exit() -> Result<ForkRes> {
    match unsafe { nix::unistd::fork() } {
        Result::Ok(ForkResult::Parent { child, .. }) => {
            let wait_res = nix::sys::wait::waitpid(child, None)?;
            Ok(ForkRes::Parent(wait_res))
        }
        Result::Ok(ForkResult::Child) => Ok(ForkRes::Child),
        Err(e) => Err(e.into()),
    }
}

struct ContainerLocation {
    upper: PathBuf,
    merged: PathBuf,
    work: PathBuf,
}

enum ForkRes {
    Child,
    Parent(WaitStatus),
}
struct ProcInfo {
    proc_id: Pid,
    parent_id: Pid,
    u_id: Uid,
    g_id: Gid,
    host_name: String,
    cwd: String,
}
