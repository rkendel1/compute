use std::env;
use std::fs;
use std::io::{self, Read};
use std::net::TcpStream;
use std::process;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    match args[1].as_str() {
        "stdin" => {
            let mut data = Vec::new();
            io::stdin().read_to_end(&mut data).unwrap();
            std::io::Write::write_all(&mut io::stdout(), &data).unwrap();
        }
        "exit" => process::exit(7),
        "sleep" => thread::sleep(Duration::from_secs(5)),
        "memory" => {
            let data = vec![0x5a_u8; 64 * 1024 * 1024];
            println!("{}", data[data.len() - 1]);
        }
        "filesystem" => {
            println!("{}", if fs::read("/etc/passwd").is_err() { "blocked" } else { "visible" });
        }
        "network" => {
            println!("{}", if TcpStream::connect("127.0.0.1:9").is_err() { "blocked" } else { "visible" });
        }
        "certify" => {
            let value = |name: &str| env::var(name).unwrap_or_else(|_| "missing".into());
            let input = fs::read_to_string(format!("{}/hello.txt", value("COMPUTE_WORK_DIR"))).unwrap();
            let result = format!(
                "{{\"input\":\"{}\",\"success\":true,\"argument\":\"{}\",\"environment\":\"{}\",\"host_environment\":\"{}\"}}",
                input, args[2], value("CERTIFICATION_ENV"), value("COMPUTE_HOST_SECRET")
            );
            fs::write(format!("{}/result.json", value("COMPUTE_OUTPUT_DIR")), result).unwrap();
            println!(
                "{{\"runtime\":\"{}\",\"runtime_version\":\"{}\"}}",
                env!("CERTIFICATION_RUNTIME"),
                env!("CERTIFICATION_VERSION")
            );
            eprintln!("certification-stderr");
        }
        _ => process::exit(2),
    }
}
