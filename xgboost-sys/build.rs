extern crate bindgen;

use std::process::Command;
use std::env;
use std::path::{Path, PathBuf};
use std::fs::create_dir;

fn main() {
    let target = env::var("TARGET").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
    let xgb_root = Path::new(&out_dir).join("xgboost");

    // copy source code into OUT_DIR for compilation if it doesn't exist
    if !xgb_root.exists() {
        Command::new("cp")
            .args(&["-r", "xgboost", xgb_root.to_str().unwrap()])
            .status()
            .unwrap_or_else(|e| {
                panic!("Failed to copy ./xgboost to {}: {}", xgb_root.display(), e);
            });
    }

    if !xgb_root.join("build").exists() {
        let build_dir = xgb_root.join("build");
        create_dir(&build_dir).expect("Failed to create xgboost build directory");
        
        Command::new("cmake")
            .arg("..")
            .arg("-DHIDE_CXX_SYMBOLS=ON")
            .arg("-DCMAKE_INSTALL_PREFIX=out")
            .current_dir(&build_dir)
            .status()
            .expect("Failed to execute XGBoost cmake");

        Command::new("make")
            .arg("install")
            .current_dir(&build_dir)
            .status()
            .expect("Failed to execute XGBoost make");
    }
    /*
    // TODO: allow for dynamic/static linking
    // TODO: check whether rabit should be built/linked
    if !xgb_root.join("lib").exists() {
        // TODO: better checks for build completion, currently xgboost's build script can run
        // `make clean_all` if openmp build fails
        Command::new(xgb_root.join("build.sh"))
            .current_dir(&xgb_root)
            .status()
            .expect("Failed to execute XGBoost build.sh script.");
    }
    */

    let xgb_root = xgb_root.canonicalize().unwrap();

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", xgb_root.join("build/out/include").display()))
        .size_t_is_usize(true)
        .generate()
        .expect("Unable to generate bindings.");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings.");

    println!("cargo:rustc-link-search={}", xgb_root.join("build/out/lib").display());
    //println!("cargo:rustc-link-search={}", xgb_root.join("dmlc-core").display());

    // check if built with multithreading support, otherwise link to dummy lib
    /*
    if xgb_root.join("rabit/lib/librabit.a").exists() {
        println!("cargo:rustc-link-lib=static=rabit");
        println!("cargo:rustc-link-lib=dylib=gomp");
    } else {
        println!("cargo:rustc-link-lib=static=rabit_empty");
    }
    */

    // link to appropriate C++ lib
    if target.contains("apple") {
        println!("cargo:rustc-link-lib=c++");
    } else {
    }
    println!("cargo:rustc-link-lib=stdc++");

    println!("cargo:rustc-link-lib=dylib=dmlc");
    println!("cargo:rustc-link-lib=dylib=xgboost");
}
