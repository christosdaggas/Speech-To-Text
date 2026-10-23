use std::env;
use std::path::PathBuf;
use std::process::Command;

fn log_tool_version(tool: &str, version_arg: &str) {
    match Command::new(tool).arg(version_arg).output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let first = text.lines().next().unwrap_or("").trim();
            println!("cargo:warning=build tool {tool}: {first}");
        }
        _ => println!("cargo:warning=build tool {tool}: not found"),
    }
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    cc::Build::new()
        .file("native/parakeet.c")
        .warnings(true)
        .flag_if_supported("-std=c11")
        .compile("stt_parakeet");
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rerun-if-changed=native/parakeet.c");
    println!("cargo:rerun-if-changed=native/parakeet-abi.h");

    log_tool_version("glib-compile-resources", "--version");
    log_tool_version("msgfmt", "--version");

    let data_dir = manifest_dir.join("data").join("resources");
    let gresource_xml = data_dir.join("com.chrisdaggas.speech-to-text.gresource.xml");
    let output = out_dir.join("speech-to-text.gresource");

    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(&data_dir)
        .arg("--target")
        .arg(&output)
        .arg(&gresource_xml)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rerun-if-changed=data/resources/com.chrisdaggas.speech-to-text.gresource.xml");
            println!("cargo:rerun-if-changed=data/resources/style.css");
            println!("cargo:rustc-cfg=feature=\"gresource\"");
        }
        _ => {
            eprintln!("Warning: glib-compile-resources not available, using fallback CSS loading");
        }
    }

    let po_dir = manifest_dir.join("po");
    if po_dir.exists() {
        let locales = ["de", "el", "es", "fr", "it", "pt", "ru", "zh"];

        for locale in &locales {
            let po_file = po_dir.join(format!("{}.po", locale));
            if po_file.exists() {
                let mo_dir = out_dir.join("locale").join(locale).join("LC_MESSAGES");
                std::fs::create_dir_all(&mo_dir).ok();
                let mo_file = mo_dir.join("speech-to-text.mo");

                let result = Command::new("msgfmt")
                    .args([po_file.to_str().unwrap(), "-o", mo_file.to_str().unwrap()])
                    .status();

                match result {
                    Ok(s) if s.success() => {}
                    _ => {
                        println!(
                            "cargo:warning=msgfmt failed for {}.po (gettext tools not installed?)",
                            locale
                        );
                    }
                }
            }
            println!("cargo:rerun-if-changed=po/{}.po", locale);
        }

        // Release binaries must use installed assets, never a checkout path.
        if env::var("PROFILE").as_deref() == Ok("debug") {
            println!(
                "cargo:rustc-env=LOCALE_DIR={}",
                out_dir.join("locale").display()
            );
        }
    }

    // Same source-tree convenience for the bundled Silero VAD model: debug
    // builds find it in data/vad, release builds use the packaged path.
    if env::var("PROFILE").as_deref() == Ok("debug") {
        println!(
            "cargo:rustc-env=VAD_DIR={}",
            manifest_dir.join("data").join("vad").display()
        );
    }
    println!("cargo:rerun-if-changed=data/vad/ggml-silero-v5.1.2.bin");
}
