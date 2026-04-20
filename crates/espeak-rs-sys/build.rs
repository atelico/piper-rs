use cmake::Config;
use glob::glob;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var("BUILD_DEBUG").is_ok() {
            println!("cargo:warning=[DEBUG] {}", format!($($arg)*));
        }
    };
}

fn get_cargo_target_dir() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let profile = std::env::var("PROFILE")?;
    let mut target_dir = None;
    let mut sub_path = out_dir.as_path();
    while let Some(parent) = sub_path.parent() {
        if parent.ends_with(&profile) {
            target_dir = Some(parent);
            break;
        }
        sub_path = parent;
    }
    let target_dir = target_dir.ok_or("not found")?;
    Ok(target_dir.to_path_buf())
}

fn copy_folder(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("Failed to create dst directory");
    if cfg!(unix) {
        std::process::Command::new("cp")
            .arg("-rf")
            .arg(src)
            .arg(dst.parent().unwrap())
            .status()
            .expect("Failed to execute cp command");
    }

    if cfg!(windows) {
        std::process::Command::new("robocopy.exe")
            .arg("/e")
            .arg(src)
            .arg(dst)
            .status()
            .expect("Failed to execute robocopy command");
    }
}

/// Drive a separate cmake build of espeak-ng for the HOST architecture to
/// produce the phoneme data files (phondata, phontab, phonindex, per-language
/// dicts). Used on iOS cross-compile because the iOS-arm64 binary can't be
/// executed on the host to generate these files at build time.
///
/// Outputs end up in `<out_dir>/espeak-ng-data-host/`. Callers emit this path
/// as `cargo:data-dir=...` so downstream crates can stage it into the iOS
/// app's resource bundle. At runtime, the app passes the bundle path to
/// `espeak_ng_InitializePath()` to override the compile-time `path_home`.
fn build_host_espeak_data(espeak_dst: &Path, out_dir: &Path, n_path_home: &str) {
    let host_build = out_dir.join("espeak-ng-host-build");
    let host_install = out_dir.join("espeak-ng-host-install");
    let data_dst = out_dir.join("espeak-ng-data-host");

    // If we've already produced the data files, nothing to do.
    if data_dst.join("phondata").exists() {
        debug_log!("host espeak data already present: {:?}", data_dst);
        return;
    }

    // Reuse the espeak-ng source tree the main flow already copied to
    // `$OUT_DIR/espeak-ng`. Host and target builds share sources but write to
    // separate build dirs so artifacts don't collide.
    assert!(
        espeak_dst.join("CMakeLists.txt").exists(),
        "espeak source missing CMakeLists.txt at {:?}",
        espeak_dst
    );
    std::fs::create_dir_all(&host_build).expect("create host_build");

    // Configure host build. We deliberately do NOT pass --target / -isysroot
    // etc., because we want the compiler to produce host-native binaries.
    let cflags = format!("-DN_PATH_HOME={} -w", n_path_home);
    let status = Command::new("cmake")
        .arg(espeak_dst)
        .arg("-B")
        .arg(&host_build)
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg("-DUSE_LIBPCAUDIO=OFF")
        .arg("-DENABLE_TESTS=OFF")
        .arg("-DCOMPILE_INTONATIONS=ON")
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!("-DCMAKE_INSTALL_PREFIX={}", host_install.display()))
        .arg(format!("-DCMAKE_C_FLAGS={}", cflags))
        .arg(format!("-DCMAKE_CXX_FLAGS={}", cflags))
        // Apple silicon explicit — cmake otherwise picks this up from the
        // environment, but we're not inheriting the cross-compile env here.
        .env_remove("SDKROOT")
        .env_remove("CFLAGS")
        .env_remove("CXXFLAGS")
        .env_remove("TARGET")
        .status()
        .expect("failed to run host cmake configure");
    assert!(status.success(), "host cmake configure failed");

    let status = Command::new("cmake")
        .arg("--build")
        .arg(&host_build)
        .arg("--target")
        .arg("data")
        .arg("--config")
        .arg("Release")
        .arg("--parallel")
        .env_remove("SDKROOT")
        .status()
        .expect("failed to run host cmake build");
    assert!(status.success(), "host cmake --build data failed");

    // The `data` target writes into `<host_build>/espeak-ng-data/`. Copy the
    // relevant files into `data_dst` so downstream consumers have a stable
    // location to read from.
    let src_data = host_build.join("espeak-ng-data");
    if !src_data.join("phondata").exists() {
        panic!(
            "host build did not produce phondata at {:?}; contents: {:?}",
            src_data,
            std::fs::read_dir(&src_data)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.file_name())).collect::<Vec<_>>())
                .unwrap_or_default()
        );
    }
    std::fs::create_dir_all(&data_dst).expect("create data_dst");
    copy_folder_contents(&src_data, &data_dst);
    debug_log!("host espeak-ng-data -> {:?}", data_dst);
}

/// Recursive copy of `src`'s contents into `dst` (does not copy `src` itself).
fn copy_folder_contents(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).ok();
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_folder_contents(&from, &to);
        } else {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

fn extract_lib_names(out_dir: &Path, build_shared_libs: bool) -> Vec<String> {
    let lib_pattern = if cfg!(windows) {
        "*.lib"
    } else if cfg!(target_os = "macos") {
        if build_shared_libs {
            "*.dylib"
        } else {
            "*.a"
        }
    } else {
        if build_shared_libs {
            "*.so"
        } else {
            "*.a"
        }
    };
    let libs_dir = out_dir.join("lib");
    let pattern = libs_dir.join(lib_pattern);
    debug_log!("Extract libs {}", pattern.display());

    let mut lib_names: Vec<String> = Vec::new();

    // Process the libraries based on the pattern
    for entry in glob(pattern.to_str().unwrap()).unwrap() {
        match entry {
            Ok(path) => {
                let stem = path.file_stem().unwrap();
                let stem_str = stem.to_str().unwrap();

                // Remove the "lib" prefix if present
                let lib_name = if stem_str.starts_with("lib") {
                    stem_str.strip_prefix("lib").unwrap_or(stem_str)
                } else {
                    stem_str
                };
                lib_names.push(lib_name.to_string());
            }
            Err(e) => println!("cargo:warning=error={}", e),
        }
    }
    lib_names
}

fn extract_lib_assets(out_dir: &Path) -> Vec<PathBuf> {
    let shared_lib_pattern = if cfg!(windows) {
        "*.dll"
    } else if cfg!(target_os = "macos") {
        "*.dylib"
    } else {
        "*.so"
    };

    let libs_dir = out_dir.join("lib");
    let pattern = libs_dir.join(shared_lib_pattern);
    debug_log!("Extract lib assets {}", pattern.display());
    let mut files = Vec::new();

    for entry in glob(pattern.to_str().unwrap()).unwrap() {
        match entry {
            Ok(path) => {
                files.push(path);
            }
            Err(e) => eprintln!("cargo:warning=error={}", e),
        }
    }

    files
}

fn macos_link_search_path() -> Option<String> {
    let output = Command::new("clang")
        .arg("--print-search-dirs")
        .output()
        .ok()?;
    if !output.status.success() {
        println!(
            "failed to run 'clang --print-search-dirs', continuing without a link search path"
        );
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("libraries: =") {
            let path = line.split('=').nth(1)?;
            return Some(format!("{}/lib/darwin", path));
        }
    }

    println!("failed to determine link search path, continuing without it");
    None
}

fn main() {
    println!("cargo:rustc-link-lib=speechPlayer");
    println!("cargo:rustc-link-lib=espeak-ng");
    println!("cargo:rustc-link-lib=ucd");
    let target = env::var("TARGET").unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let target_dir = get_cargo_target_dir().unwrap();
    let espeak_dst = out_dir.join("espeak-ng");
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("Failed to get CARGO_MANIFEST_DIR");
    let espeak_src = Path::new(&manifest_dir).join("espeak-ng");
    let build_shared_libs = false;

    let build_shared_libs = std::env::var("ESPEAK_BUILD_SHARED_LIBS")
        .map(|v| v == "1")
        .unwrap_or(build_shared_libs);
    let profile = env::var("ESPEAK_LIB_PROFILE").unwrap_or("Release".to_string());
    let static_crt = env::var("ESPEAK_STATIC_CRT")
        .map(|v| v == "1")
        .unwrap_or(false);

    debug_log!("TARGET: {}", target);
    debug_log!("CARGO_MANIFEST_DIR: {}", manifest_dir);
    debug_log!("TARGET_DIR: {}", target_dir.display());
    debug_log!("OUT_DIR: {}", out_dir.display());
    debug_log!("BUILD_SHARED: {}", build_shared_libs);

    // Prepare espeak-ng source
    if !espeak_dst.exists() {
        debug_log!("Copy {} to {}", espeak_src.display(), espeak_dst.display());
        copy_folder(&espeak_src, &espeak_dst);
    }
    // Speed up build
    env::set_var(
        "CMAKE_BUILD_PARALLEL_LEVEL",
        std::thread::available_parallelism()
            .unwrap()
            .get()
            .to_string(),
    );

    // Bindings
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", espeak_dst.display()))
        .clang_arg(format!(
            "-I{}",
            espeak_dst.join("src").join("include").display()
        ))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Failed to generate bindings");

    // Write the generated bindings to an output file
    let bindings_path = out_dir.join("bindings.rs");
    bindings
        .write_to_file(bindings_path)
        .expect("Failed to write bindings");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=./espeak-ng");

    debug_log!("Bindings Created");

    // Build with Cmake

    let mut config = Config::new(&espeak_dst);

    config.define(
        "BUILD_SHARED_LIBS",
        if build_shared_libs { "ON" } else { "OFF" },
    );

    if cfg!(windows) {
        config.static_crt(static_crt);
    }

    if cfg!(target_os = "macos") {
        config.define("USE_LIBPCAUDIO", "OFF");
    }

    // General
    config
        .profile(&profile)
        .define("ENABLE_TESTS", "OFF")
        .define(
            "COMPILE_INTONATIONS",
            if cfg!(feature = "compile-espeak-intonations") {
                "ON"
            } else {
                "OFF"
            },
        )
        .very_verbose(std::env::var("CMAKE_VERBOSE").is_ok()) // Not verbose by default
        .always_configure(false);

    // Raise espeak-ng's internal `N_PATH_HOME` buffer from the upstream
    // default (160 on POSIX, 230 on Windows) to 4096. The buffer stores
    // `PATH_ESPEAK_DATA` at startup, and on cargo builds the baked-in path
    // routinely exceeds 160 chars (e.g. `<workspace>/target/<profile>/build/
    // espeak-rs-sys-<hash>/out/share/espeak-ng-data`), which truncates the
    // path at runtime and cascades into "Failed to open" / "Bad vowel file"
    // failures during phoneme-data compilation. espeak-ng already defines
    // N_PATH_HOME via `#ifndef`, so an external `-D` override is the
    // sanctioned way to tune this. `ESPEAK_N_PATH_HOME` lets consumers pick
    // a different size (e.g. lower for memory-constrained embedded builds).
    //
    // `cflag`/`cxxflag` append to cmake-rs's computed CMAKE_{C,CXX}_FLAGS so
    // we don't clobber `-ffunction-sections -fPIC --target=... -w` etc.
    let n_path_home = std::env::var("ESPEAK_N_PATH_HOME").unwrap_or_else(|_| "4096".to_string());
    let flag = format!("-DN_PATH_HOME={}", n_path_home);
    config.cflag(&flag);
    config.cxxflag(&flag);
    debug_log!("N_PATH_HOME override: {}", n_path_home);

    // ---------------------------------------------------------------------
    // iOS cross-compile handling.
    //
    // Two problems arise when targeting aarch64-apple-ios from a mac host:
    //
    //   1. cmake with CMAKE_SYSTEM_NAME=iOS turns MACOSX_BUNDLE ON by default
    //      for executable targets. espeak-ng's src/CMakeLists.txt does a
    //      bare `install(TARGETS espeak-ng-bin)` with no BUNDLE DESTINATION,
    //      which fails at configure time on iOS.
    //
    //   2. Even if configure succeeds, the `data` target invokes the
    //      freshly-built espeak-ng binary with --compile-phonemes / --compile=<lang>
    //      to generate `phondata`, `phontab`, `phonindex` and per-language dicts.
    //      On cross-compile that binary is iOS-arm64 and can't exec on the host
    //      → build fails.
    //
    // Fix:
    //   - Force MACOSX_BUNDLE off globally so the bare `install(TARGETS)` line
    //     no longer trips the "no BUNDLE DESTINATION" check.
    //   - Build only the `espeak-ng` static library target (skip binary + data)
    //     for the iOS target — we only need libespeak-ng.a to link into the app.
    //   - Run a separate HOST cmake build (macos, same arch as builder) to
    //     produce the espeak-ng-data directory, and expose that path as
    //     `cargo:data-dir=<...>` so downstream crates can bundle it into the
    //     iOS app resources. At runtime the app passes this path to
    //     espeak_ng_InitializePath() before any phonemize call.
    //
    // Non-iOS targets are unaffected: they take the existing config.build()
    // path and get the full library + binary + data in one cmake run.
    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();
    let is_ios = target.contains("apple-ios");
    let is_cross = target != host;

    let bindings_dir = if is_ios {
        config.define("CMAKE_MACOSX_BUNDLE", "OFF");
        // spect.c branches on `#ifdef HAVE_SYS_ENDIAN_H` to pick its source of
        // le16toh/le32toh. espeak-ng's cmake never sets this, so on hosts
        // where <endian.h> doesn't define those macros (iOS SDK is one such)
        // the build fails with "call to undeclared function 'le16toh'".
        // The iOS SDK does ship `<sys/endian.h>` with the needed macros, so
        // forcing this define selects the right header.
        config.cflag("-DHAVE_SYS_ENDIAN_H=1");
        config.cxxflag("-DHAVE_SYS_ENDIAN_H=1");
        // Build only the static library target; don't attempt to install the
        // binary (which would fail on iOS) and don't generate data (requires
        // running the binary on host, handled separately below).
        config.build_target("espeak-ng");
        let target_install = config.build();

        if is_cross {
            build_host_espeak_data(&espeak_dst, &out_dir, &n_path_home);
            let host_data_dir = out_dir.join("espeak-ng-data-host");
            assert!(
                host_data_dir.join("phondata").exists(),
                "host espeak-ng build did not produce phondata at {:?}",
                host_data_dir
            );
            println!("cargo:data-dir={}", host_data_dir.display());
            debug_log!("host data dir: {}", host_data_dir.display());
        }
        target_install
    } else {
        config.build()
    };

    // Search paths
    println!("cargo:rustc-link-search={}", out_dir.join("lib").display());
    println!(
        "cargo:rustc-link-search={}",
        out_dir.join("build/src/speechPlayer").display()
    );
    println!(
        "cargo:rustc-link-search={}",
        out_dir.join("build/src/ucd-tools").display()
    );
    // iOS builds use `config.build_target("espeak-ng")` (no install step), so
    // libespeak-ng.a stays in the build tree instead of landing in $OUT_DIR/lib.
    if is_ios {
        println!(
            "cargo:rustc-link-search={}",
            out_dir.join("build/src/libespeak-ng").display()
        );
    }
    println!("cargo:rustc-link-search={}", bindings_dir.display());

    if cfg!(windows) {
        println!(
            "cargo:rustc-link-search={}",
            out_dir.join("build/src/speechPlayer/Release").display()
        );
        println!(
            "cargo:rustc-link-search={}",
            out_dir.join("build/src/ucd-tools/Release").display()
        );
    }

    // macOS
    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=c++");
    }

    // Link libraries
    let espeak_libs_kind = if build_shared_libs { "dylib" } else { "static" };
    let espeak_libs = extract_lib_names(&out_dir, build_shared_libs);

    for lib in espeak_libs {
        debug_log!(
            "LINK {}",
            format!("cargo:rustc-link-lib={}={}", espeak_libs_kind, lib)
        );
        println!(
            "{}",
            format!("cargo:rustc-link-lib={}={}", espeak_libs_kind, lib)
        );
    }

    // Windows debug
    if cfg!(all(debug_assertions, windows)) {
        println!("cargo:rustc-link-lib=dylib=msvcrtd");
    }

    // Linux
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }

    if target.contains("apple") {
        // On Apple targets we link against the clang compiler runtime to get
        // helpers like `__chkstk_darwin` (stack probes, inserted when a frame
        // uses >4KB, which happens with N_PATH_HOME=4096). The library ships
        // per-platform: libclang_rt.osx.a on macOS, libclang_rt.ios.a on iOS.
        // Mixing arches is a hard link error, so pick the right one.
        //
        // More details at https://github.com/alexcrichton/curl-rust/issues/279.
        if let Some(path) = macos_link_search_path() {
            let rt_lib = if is_ios { "clang_rt.ios" } else { "clang_rt.osx" };
            println!("cargo:rustc-link-lib={}", rt_lib);
            println!("cargo:rustc-link-search={}", path);
        }
    }

    // copy DLLs to target
    if build_shared_libs {
        let libs_assets = extract_lib_assets(&out_dir);
        for asset in libs_assets {
            let asset_clone = asset.clone();
            let filename = asset_clone.file_name().unwrap();
            let filename = filename.to_str().unwrap();
            let dst = target_dir.join(filename);
            debug_log!("HARD LINK {} TO {}", asset.display(), dst.display());
            if !dst.exists() {
                std::fs::hard_link(asset.clone(), dst).unwrap();
            }

            // Copy DLLs to examples as well
            if target_dir.join("examples").exists() {
                let dst = target_dir.join("examples").join(filename);
                debug_log!("HARD LINK {} TO {}", asset.display(), dst.display());
                if !dst.exists() {
                    std::fs::hard_link(asset.clone(), dst).unwrap();
                }
            }

            // Copy DLLs to target/profile/deps as well for tests
            let dst = target_dir.join("deps").join(filename);
            debug_log!("HARD LINK {} TO {}", asset.display(), dst.display());
            if !dst.exists() {
                std::fs::hard_link(asset.clone(), dst).unwrap();
            }
        }
    }
}
