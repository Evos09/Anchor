//! One attack per invocation: `probe <attack> [args...]`. Prints
//! `BLOCKED <detail>` when the platform stopped it, `ALLOWED <detail>`
//! when it went through. Limits that kill the process (CPU, memory, file
//! size) show up in the exit status instead.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn verdict(ok: bool, detail: impl std::fmt::Display) {
    println!("{} {detail}", if ok { "ALLOWED" } else { "BLOCKED" });
    let _ = std::io::stdout().flush();
}

fn connect(addr: &str) {
    let a: SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => return verdict(false, format!("bad address: {e}")),
    };
    match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
        Ok(_) => verdict(true, format!("connected to {a}")),
        Err(e) => verdict(false, e),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    match arg(0).as_str() {
        "connect" => connect(&arg(1)),
        "read" => match std::fs::read(arg(1)) {
            Ok(b) => verdict(true, format!("read {} bytes", b.len())),
            Err(e) => verdict(false, e),
        },
        "write" => match std::fs::write(arg(1), b"planted by the probe") {
            Ok(()) => verdict(true, "wrote the file"),
            Err(e) => verdict(false, e),
        },
        "list" => match std::fs::read_dir(arg(1)) {
            Ok(d) => verdict(true, format!("listed {} entries", d.count())),
            Err(e) => verdict(false, e),
        },
        "spawn-many" => {
            // Keep every child alive; the run kills the tree afterwards.
            let me = std::env::current_exe().unwrap_or_default();
            let mut kids = Vec::new();
            let limit: usize = arg(1).parse().unwrap_or(64);
            let err = loop {
                if kids.len() >= limit {
                    break None;
                }
                match std::process::Command::new(&me).arg("sleep").spawn() {
                    Ok(c) => kids.push(c),
                    Err(e) => break Some(e),
                }
            };
            match err {
                None => verdict(true, format!("spawned {}", kids.len())),
                Some(e) => verdict(false, format!("spawned {} then: {e}", kids.len())),
            }
        }
        "sleep" => std::thread::sleep(Duration::from_secs(60)),
        "alloc" => {
            let n: usize = arg(1).parse().unwrap_or(1 << 30);
            let mut v: Vec<u8> = Vec::new();
            match v.try_reserve_exact(n) {
                Err(e) => verdict(false, e),
                Ok(()) => {
                    // Touch every page so resident memory really grows.
                    v.resize(n, 1);
                    verdict(true, format!("allocated {n} bytes"));
                }
            }
        }
        "spin" => {
            let mut x: u64 = 0;
            loop {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        }
        "big-file" => {
            let n: usize = arg(2).parse().unwrap_or(8 << 20);
            let r = std::fs::File::create(arg(1)).and_then(|mut f| {
                let chunk = vec![0u8; 1 << 20];
                let mut done = 0;
                while done < n {
                    f.write_all(&chunk)?;
                    done += chunk.len();
                }
                f.sync_all()
            });
            match r {
                Ok(()) => verdict(true, format!("wrote {n} bytes")),
                Err(e) => verdict(false, e),
            }
        }
        "env" => {
            let names: Vec<String> = std::env::vars_os()
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
            println!("ENV {}", names.join(","));
        }
        #[cfg(unix)]
        "unix-connect" => match std::os::unix::net::UnixStream::connect(arg(1)) {
            Ok(_) => verdict(true, "connected"),
            Err(e) => verdict(false, e),
        },
        #[cfg(unix)]
        "signal" => {
            let pid: i32 = arg(1).parse().unwrap_or(0);
            // SAFETY: signal 0 only checks permission.
            let r = unsafe { libc::kill(pid, 0) };
            if r == 0 {
                verdict(true, format!("may signal {pid}"))
            } else {
                verdict(false, std::io::Error::last_os_error())
            }
        }
        #[cfg(target_os = "linux")]
        "syscall" => {
            let (nr, a1) = match arg(1).as_str() {
                "ptrace" => (libc::SYS_ptrace, libc::PTRACE_TRACEME as libc::c_long),
                "bpf" => (libc::SYS_bpf, 0),
                "keyctl" => (libc::SYS_keyctl, 0),
                "mount" => (libc::SYS_mount, 0),
                "unshare" => (libc::SYS_unshare, libc::CLONE_NEWUSER as libc::c_long),
                other => return verdict(false, format!("unknown syscall {other}")),
            };
            // SAFETY: each call fails harmlessly or is refused by the filter;
            // invalid pointer arguments are null.
            let r = unsafe { libc::syscall(nr, a1, 0, 0, 0, 0) };
            let e = std::io::Error::last_os_error();
            if r == -1 && e.raw_os_error() == Some(libc::EPERM) {
                verdict(false, "EPERM")
            } else {
                verdict(true, format!("returned {r} ({e})"))
            }
        }
        other => {
            eprintln!("unknown attack {other:?}");
            std::process::exit(2);
        }
    }
}
