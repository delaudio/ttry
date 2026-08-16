use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "echo".into());
    match mode.as_str() {
        "info" => println!(
            "TERM={} SIZE={}x{}",
            std::env::var("TERM").unwrap_or_default(),
            std::env::var("COLUMNS").unwrap_or_default(),
            std::env::var("LINES").unwrap_or_default()
        ),
        "delayed" => {
            print!("loading");
            io::stdout().flush().unwrap();
            thread::sleep(Duration::from_millis(80));
            print!("\rready  ");
            io::stdout().flush().unwrap();
        }
        "hang" => {
            #[cfg(unix)]
            unsafe {
                nix::libc::signal(nix::libc::SIGTERM, nix::libc::SIG_IGN);
            }
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        #[cfg(unix)]
        "tree" => {
            let mut child = std::process::Command::new("sh")
                .args(["-c", "sleep 60"])
                .spawn()
                .expect("spawn descendant");
            println!("DESCENDANT_PID={}", child.id());
            io::stdout().flush().unwrap();
            let _ = child.wait();
        }
        "resize" => resize_fixture(),
        "fail" => std::process::exit(7),
        _ => echo_loop(),
    }
}

fn resize_fixture() {
    println!("SIZE:{}", terminal_size());
    io::stdout().flush().unwrap();
    let mut byte = [0_u8; 1];
    let _ = io::stdin().read(&mut byte);
    println!("RESIZED:{}", terminal_size());
    io::stdout().flush().unwrap();
}

#[cfg(unix)]
fn terminal_size() -> String {
    let mut size = nix::libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result =
        unsafe { nix::libc::ioctl(nix::libc::STDOUT_FILENO, nix::libc::TIOCGWINSZ, &mut size) };
    if result == 0 {
        format!("{}x{}", size.ws_col, size.ws_row)
    } else {
        "unknown".into()
    }
}

#[cfg(not(unix))]
fn terminal_size() -> String {
    "unsupported".into()
}

fn echo_loop() {
    print!("READY\r\n");
    io::stdout().flush().unwrap();
    let mut input = [0_u8; 128];
    loop {
        match io::stdin().read(&mut input) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                if input[..count].contains(&3) {
                    break;
                }
                print!("INPUT:");
                io::stdout().write_all(&input[..count]).unwrap();
                io::stdout().flush().unwrap();
            }
        }
    }
}
