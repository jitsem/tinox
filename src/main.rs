use std::{convert::Infallible, ffi::CString, fs, str::FromStr};

use anyhow::{Context, Ok, Result};
use nix::{
    mount::MsFlags,
    sched::CloneFlags,
    sys::wait::WaitStatus,
    unistd::{ForkResult, Gid, Pid, Uid},
};

fn main() -> Result<()> {
    print!("Starting tinox...");
    let tinox_info = get_process()?;
    print_process(&tinox_info);
    setup_parent_namespaces()?;
    map_uid_gid(&tinox_info.u_id, &tinox_info.g_id)?;
    match fork_and_wait_for_exit()? {
        ForkRes::Child => {
            isolate_child()?;
            change_hostname()?;
            change_filesystem()?;
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

fn change_filesystem() -> Result<()> {
    nix::mount::mount(
        Some("fs/fs"),
        "fs/box",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .context("Failed to mount filesystem")?;
    //TODO, i'm told pivot_root is better?
    nix::unistd::chroot("fs/box").context("Could not chroot into mounted folder")?;
    nix::unistd::chdir("/").context("Failed to change to root directory")
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
    nix::sched::unshare(CloneFlags::CLONE_NEWNS).context("Failed to isolate mount namespace")
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
