fn main() {
    tauri_build::build();

    // `winres` build dep is available for embedding Windows app manifest/icons.
    // Currently the Tauri build handles manifest embedding; enable winres.compile()
    // here when a standalone .manifest file is added.
}
