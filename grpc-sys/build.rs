// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::HashSet;
use std::env::VarError;
use std::io::prelude::*;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use cmake::Config as CmakeConfig;
use pkg_config::{Config as PkgConfig, Library};
use walkdir::WalkDir;

const GRPC_VERSION: &str = "1.35.0";

fn probe_library(library: &str, cargo_metadata: bool) -> Library {
    match PkgConfig::new()
        .atleast_version(GRPC_VERSION)
        .cargo_metadata(cargo_metadata)
        .probe(library)
    {
        Ok(lib) => lib,
        Err(e) => panic!("can't find library {} via pkg-config: {:?}", library, e),
    }
}

fn prepare_grpc() {
    let modules = vec![
        "grpc",
        "grpc/third_party/cares/cares",
        "grpc/third_party/address_sorting",
        "grpc/third_party/abseil-cpp",
        "grpc/third_party/re2",
    ];

    for module in modules {
        if is_directory_empty(module).unwrap_or(true) {
            panic!(
                "Can't find module {}. You need to run `git submodule \
                 update --init --recursive` first to build the project.",
                module
            );
        }
    }
}

fn is_directory_empty<P: AsRef<Path>>(p: P) -> Result<bool, io::Error> {
    let mut entries = fs::read_dir(p)?;
    Ok(entries.next().is_none())
}

fn trim_start<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.starts_with(prefix) {
        Some(s.trim_start_matches(prefix))
    } else {
        None
    }
}

/// If cache is stale, remove it to avoid compilation failure.
fn clean_up_stale_cache(cxx_compiler: String) {
    // We don't know the cmake output path before it's configured.
    let build_dir = format!("{}/build", env::var("OUT_DIR").unwrap());
    let path = format!("{}/CMakeCache.txt", build_dir);
    let f = match std::fs::File::open(&path) {
        Ok(f) => BufReader::new(f),
        // It may be an empty directory.
        Err(_) => return,
    };
    let cache_stale = f.lines().any(|l| {
        let l = l.unwrap();
        trim_start(&l, "CMAKE_CXX_COMPILER:").map_or(false, |s| {
            let mut splits = s.splitn(2, '=');
            splits.next();
            splits.next().map_or(false, |p| p != cxx_compiler)
        })
    });
    // CMake can't handle compiler change well, it will invalidate cache without respecting command
    // line settings and result in configuration failure.
    // See https://gitlab.kitware.com/cmake/cmake/-/issues/18959.
    if cache_stale {
        let _ = fs::remove_dir_all(&build_dir);
    }
}

fn build_grpc(cc: &mut cc::Build, library: &str) {
    prepare_grpc();

    let target = env::var("TARGET").unwrap();
    let dst = {
        let mut config = CmakeConfig::new("grpc");

        if get_env("CARGO_CFG_TARGET_OS").map_or(false, |s| s == "macos") {
            config.cxxflag("-stdlib=libc++");
        }

        // Ensure CoreFoundation be found in macos or ios
        if get_env("CARGO_CFG_TARGET_OS").map_or(false, |s| s == "macos")
            || get_env("CARGO_CFG_TARGET_OS").map_or(false, |s| s == "ios")
        {
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
        }

        let cxx_compiler = if let Some(val) = get_env("CXX") {
            config.define("CMAKE_CXX_COMPILER", val.clone());
            val
        } else if env::var("CARGO_CFG_TARGET_ENV").unwrap() == "musl" {
            config.define("CMAKE_CXX_COMPILER", "g++");
            "g++".to_owned()
        } else {
            format!("{}", cc.get_compiler().path().display())
        };
        clean_up_stale_cache(cxx_compiler);

        // Cross-compile support for iOS
        match target.as_str() {
            "aarch64-apple-ios" => {
                config
                    .define("CMAKE_OSX_SYSROOT", "iphoneos")
                    .define("CMAKE_OSX_ARCHITECTURES", "arm64");
            }
            "armv7-apple-ios" => {
                config
                    .define("CMAKE_OSX_SYSROOT", "iphoneos")
                    .define("CMAKE_OSX_ARCHITECTURES", "armv7");
            }
            "armv7s-apple-ios" => {
                config
                    .define("CMAKE_OSX_SYSROOT", "iphoneos")
                    .define("CMAKE_OSX_ARCHITECTURES", "armv7s");
            }
            "i386-apple-ios" => {
                config
                    .define("CMAKE_OSX_SYSROOT", "iphonesimulator")
                    .define("CMAKE_OSX_ARCHITECTURES", "i386");
            }
            "x86_64-apple-ios" => {
                config
                    .define("CMAKE_OSX_SYSROOT", "iphonesimulator")
                    .define("CMAKE_OSX_ARCHITECTURES", "x86_64");
            }
            _ => {}
        };

        // Allow overriding of the target passed to cmake
        // (needed for Android crosscompile)
        if let Ok(val) = env::var("CMAKE_TARGET_OVERRIDE") {
            config.target(&val);
        }

        // We don't need to generate install targets.
        config.define("gRPC_INSTALL", "false");
        // We don't need to build csharp target.
        config.define("gRPC_BUILD_CSHARP_EXT", "false");
        // We don't need to build codegen target.
        config.define("gRPC_BUILD_CODEGEN", "false");
        // We don't need to build benchmarks.
        config.define("gRPC_BENCHMARK_PROVIDER", "none");
        config.define("gRPC_SSL_PROVIDER", "package");
        if cfg!(feature = "openssl") {
            if cfg!(feature = "openssl-vendored") {
                config.register_dep("openssl");
            }
        } else {
            build_boringssl(&mut config);
        }
        if cfg!(feature = "no-omit-frame-pointer") {
            config
                .cflag("-fno-omit-frame-pointer")
                .cxxflag("-fno-omit-frame-pointer");
        }
        // Uses zlib from libz-sys.
        setup_libz(&mut config);
        config.build_target(library).uses_cxx11().build()
    };

    let lib_suffix = if target.contains("msvc") {
        ".lib"
    } else {
        ".a"
    };
    let build_dir = format!("{}/build", dst.display());
    for e in WalkDir::new(&build_dir) {
        let e = e.unwrap();
        if e.file_name().to_string_lossy().ends_with(lib_suffix) {
            println!(
                "cargo:rustc-link-search=native={}",
                e.path().parent().unwrap().display()
            );
        }
    }

    let collect = |path, to: &mut HashSet<_>| {
        let f = fs::File::open(format!("{}/libs/opt/pkgconfig/{}.pc", build_dir, path)).unwrap();
        for l in io::BufReader::new(f).lines() {
            let l = l.unwrap();
            if l.starts_with("Libs: ") {
                for lib in l.split_whitespace() {
                    if let Some(s) = lib.strip_prefix("-l") {
                        to.insert(s.to_string());
                    }
                }
            }
        }
    };
    let mut libs = HashSet::new();
    collect("gpr", &mut libs);
    collect(library, &mut libs);
    for l in libs {
        println!("cargo:rustc-link-lib=static={}", l);
    }

    if cfg!(feature = "secure") {
        if cfg!(feature = "openssl") && !cfg!(feature = "openssl-vendored") {
            figure_ssl_path(&build_dir);
        } else {
            println!("cargo:rustc-link-lib=static=ssl");
            println!("cargo:rustc-link-lib=static=crypto");
        }
    } else {
        // grpc_unsecure.pc is not accurate, see also grpc/grpc#24512.
        println!("cargo:rustc-link-lib=static=upb");
        println!("cargo:rustc-link-lib=static=cares");
        println!("cargo:rustc-link-lib=static=z");
        println!("cargo:rustc-link-lib=static=address_sorting");
    }

    cc.include("grpc/include");
}

fn figure_ssl_path(build_dir: &str) {
    let path = format!("{}/CMakeCache.txt", build_dir);
    let f = BufReader::new(std::fs::File::open(&path).unwrap());
    let mut cnt = 0;
    for l in f.lines() {
        let l = l.unwrap();
        let t = trim_start(&l, "OPENSSL_CRYPTO_LIBRARY:FILEPATH=")
            .or_else(|| trim_start(&l, "OPENSSL_SSL_LIBRARY:FILEPATH="));
        if let Some(s) = t {
            let path = Path::new(s);
            println!(
                "cargo:rustc-link-search=native={}",
                path.parent().unwrap().display()
            );
            cnt += 1;
        }
    }
    if cnt != 2 {
        panic!(
            "CMake cache invalid, file {} contains {} ssl keys!",
            path, cnt
        );
    }
    println!("cargo:rustc-link-lib=ssl");
    println!("cargo:rustc-link-lib=crypto");
}

fn build_boringssl(config: &mut CmakeConfig) {
    let boringssl_artifact = boringssl_src::Build::new().build();
    config.define(
        "OPENSSL_ROOT_DIR",
        format!("{}", boringssl_artifact.root_dir().display()),
    );
    // To avoid linking system library, set lib path explicitly.
    println!(
        "cargo:rustc-link-search=native={}",
        boringssl_artifact.lib_dir().display()
    );
}

fn setup_libz(config: &mut CmakeConfig) {
    config.define("gRPC_ZLIB_PROVIDER", "package");
    config.register_dep("Z");
    // cmake script expect libz.a being under ${DEP_Z_ROOT}/lib, but libz-sys crate put it
    // under ${DEP_Z_ROOT}/build. Append the path to CMAKE_PREFIX_PATH to get around it.
    let zlib_root = env::var("DEP_Z_ROOT").unwrap();
    let prefix_path = if let Ok(prefix_path) = env::var("CMAKE_PREFIX_PATH") {
        format!("{};{}/build", prefix_path, zlib_root)
    } else {
        format!("{}/build", zlib_root)
    };
    // To avoid linking system library, set lib path explicitly.
    println!("cargo:rustc-link-search=native={}/build", zlib_root);
    println!("cargo:rustc-link-search=native={}/lib", zlib_root);
    env::set_var("CMAKE_PREFIX_PATH", prefix_path);
}

fn get_env(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={}", name);
    match env::var(name) {
        Ok(s) => Some(s),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(s)) => {
            panic!("unrecognize env var of {}: {:?}", name, s.to_string_lossy());
        }
    }
}

// Generate the bindings to grpc C-core.
// Try to disable the generation of platform-related bindings.
#[cfg(feature = "use-bindgen")]
fn bindgen_grpc(file_path: &Path) {
    // create a config to generate binding file
    let mut config = bindgen::Builder::default();
    if cfg!(feature = "secure") {
        config = config.clang_arg("-DGRPC_SYS_SECURE");
    }

    if get_env("CARGO_CFG_TARGET_OS").map_or(false, |s| s == "windows") {
        config = config.clang_arg("-D _WIN32_WINNT=0x600");
    }

    // Search header files with API interface
    let mut headers = Vec::new();
    for result in WalkDir::new(Path::new("./grpc/include")) {
        let dent = result.expect("Error happened when search headers");
        if !dent.file_type().is_file() {
            continue;
        }
        let mut file = fs::File::open(dent.path()).expect("couldn't open headers");
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .expect("Coundn't read header content");
        if buf.contains("GRPCAPI") || buf.contains("GPRAPI") {
            headers.push(String::from(dent.path().to_str().unwrap()));
        }
    }

    // To control the order of bindings
    headers.sort();
    for path in headers {
        config = config.header(path);
    }

    println!("cargo:rerun-if-env-changed=TEST_BIND");
    let gen_tests = env::var("TEST_BIND").map_or(false, |s| s == "1");

    let cfg = config
        .header("grpc_wrap.cc")
        .clang_arg("-xc++")
        .clang_arg("-I./grpc/include")
        .clang_arg("-std=c++11")
        .rustfmt_bindings(true)
        .impl_debug(true)
        .size_t_is_usize(true)
        .disable_header_comment()
        .whitelist_function(r"\bgrpc_.*")
        .whitelist_function(r"\bgpr_.*")
        .whitelist_function(r"\bgrpcwrap_.*")
        .whitelist_var(r"\bGRPC_.*")
        .whitelist_type(r"\bgrpc_.*")
        .whitelist_type(r"\bgpr_.*")
        .whitelist_type(r"\bgrpcwrap_.*")
        .whitelist_type(r"\bcensus_context.*")
        .whitelist_type(r"\bverify_peer_options.*")
        .blacklist_type(r"(__)?pthread.*")
        .blacklist_function(r"\bgpr_mu_.*")
        .blacklist_function(r"\bgpr_cv_.*")
        .blacklist_function(r"\bgpr_once_.*")
        .blacklist_type(r"gpr_mu")
        .blacklist_type(r"gpr_cv")
        .blacklist_type(r"gpr_once")
        .constified_enum_module(r"grpc_status_code")
        .layout_tests(gen_tests)
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: false,
        });
    println!("running {}", cfg.command_line_flags().join(" "));
    cfg.generate()
        .expect("Unable to generate grpc bindings")
        .write_to_file(file_path)
        .expect("Couldn't write bindings!");
}

// Determine if need to update bindings. Supported platforms do not
// need to be updated by default unless the UPDATE_BIND is specified.
// Other platforms use bindgen to generate the bindings every time.
fn config_binding_path() {
    let target = env::var("TARGET").unwrap();
    let file_path: PathBuf = match target.as_str() {
        "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu" => {
            // Cargo treats nonexistent files changed, so we only emit the rerun-if-changed
            // directive when we expect the target-specific pre-generated binding file to be
            // present.
            println!("cargo:rerun-if-changed=bindings/{}-bindings.rs", &target);

            let file_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
                .join("bindings")
                .join(format!("{}-bindings.rs", &target));

            #[cfg(feature = "use-bindgen")]
            if env::var("UPDATE_BIND").is_ok() {
                bindgen_grpc(&file_path);
            }

            file_path
        }
        _ => {
            let file_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("grpc-bindings.rs");

            #[cfg(feature = "use-bindgen")]
            bindgen_grpc(&file_path);

            file_path
        }
    };

    println!(
        "cargo:rustc-env=BINDING_PATH={}",
        file_path.to_str().unwrap()
    );
}

fn main() {
    println!("cargo:rerun-if-changed=grpc_wrap.cc");
    println!("cargo:rerun-if-changed=grpc");
    println!("cargo:rerun-if-env-changed=UPDATE_BIND");

    // create a builder to compile grpc_wrap.cc
    let mut cc = cc::Build::new();

    let library = if cfg!(feature = "secure") {
        cc.define("GRPC_SYS_SECURE", None);
        "grpc"
    } else {
        "grpc_unsecure"
    };

    if get_env("CARGO_CFG_TARGET_OS").map_or(false, |s| s == "windows") {
        // At lease vista
        cc.define("_WIN32_WINNT", Some("0x600"));
    }

    if get_env("GRPCIO_SYS_USE_PKG_CONFIG").map_or(false, |s| s == "1") {
        // Print cargo metadata.
        let lib_core = probe_library(library, true);
        for inc_path in lib_core.include_paths {
            cc.include(inc_path);
        }
    } else {
        build_grpc(&mut cc, library);
    }

    cc.cpp(true);
    if !cfg!(target_env = "msvc") {
        cc.flag("-std=c++11");
    }
    cc.file("grpc_wrap.cc");
    cc.warnings_into_errors(true);
    cc.compile("libgrpc_wrap.a");

    config_binding_path();
}
