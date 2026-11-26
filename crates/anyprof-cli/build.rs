use std::path::PathBuf;

const MACOS_INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>dev.anyprofiler.anyprof</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>anyprof</string>
	<key>SecTaskAccess</key>
	<string>allowed</string>
</dict>
</plist>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        let plist_path = out_dir.join("anyprof-Info.plist");
        std::fs::write(&plist_path, MACOS_INFO_PLIST).expect("write anyprof-Info.plist");
        for arg in [
            "-sectcreate",
            "__TEXT",
            "__info_plist",
            &plist_path.to_string_lossy(),
        ] {
            println!("cargo:rustc-link-arg-bin=anyprof=-Xlinker");
            println!("cargo:rustc-link-arg-bin=anyprof={arg}");
        }
    }
}
