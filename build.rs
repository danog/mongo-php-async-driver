//! Picks the checked-in Zend bindings (src/zend/sys/php<ver>.rs) for the PHP that `PHP_CONFIG`
//! (default `php-config`) describes, and computes the module build id PHP checks at load time.
//! Nothing is compiled here: the extension contains no C.

use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd).args(args).output().unwrap_or_else(|e| panic!("cannot run {cmd}: {e}"));
    assert!(out.status.success(), "{cmd} {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn main() {
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");
    println!("cargo:rerun-if-env-changed=PHP");
    for v in ["php84", "php85"] {
        println!("cargo:rustc-check-cfg=cfg({v})");
    }
    let config = std::env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".into());
    let api: u32 = run(&config, &["--phpapi"]).parse().expect("php-config --phpapi");
    let version = run(&config, &["--version"]);
    let php = std::env::var("PHP").unwrap_or_else(|_| run(&config, &["--php-binary"]));
    let flags = run(&php, &["-n", "-r", "echo (int) PHP_ZTS, (int) PHP_DEBUG;"]);
    if flags != "00" {
        panic!("only NTS non-debug PHP builds are supported (PHP_ZTS/PHP_DEBUG = {flags})");
    }
    let cfg = match api {
        20240924 => "php84",
        20250925 => "php85",
        _ => panic!("unsupported PHP {version} (API {api}): supported are 8.4 and 8.5"),
    };
    println!("cargo:rustc-cfg={cfg}");
    println!("cargo:rustc-env=PHP_MODULE_API={api}");
    println!("cargo:rustc-env=PHP_BUILD_ID=API{api},NTS");
}
