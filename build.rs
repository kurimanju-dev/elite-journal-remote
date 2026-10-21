// Both roles and all build targets must use the same key.
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let path = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join(".pairing-key");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .expect("open build pairing lock");
    lock.lock().expect("lock build pairing key");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("OS random pairing key");
            for b in bytes {
                write!(file, "{b:02x}").unwrap();
            }
            file.sync_all().unwrap();
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => panic!("cannot create {}: {e}", path.display()),
    }
    let mut text = String::new();
    fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let mut valid = text.len() == 64;
    if valid {
        for byte in text.bytes() {
            if !byte.is_ascii_hexdigit() {
                valid = false;
                break;
            }
        }
    }
    assert!(valid, "invalid .pairing-key");
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rustc-env=ED_RELAY_PAIR_KEY={text}");
}
